//! Native conversion selection over the shared Catalyst entity source.
use super::ports::registry::RegistryStore;
use dcl_contents::errors::ApiError;
use dcl_contents::manifest_store::AbManifestStore;
use dcl_contents::registry::{async_trait, EntitySource};
use dcl_contents::types::{ActiveEntity, EntityStatus};
use std::sync::Arc;

pub struct RegisteredSource {
    pub content: Arc<dyn EntitySource>,
    pub registry: RegistryStore,
    pub manifests: AbManifestStore,
}

#[async_trait]
impl EntitySource for RegisteredSource {
    async fn resolve_pointers(&self, pointers: &[String]) -> Result<Vec<ActiveEntity>, ApiError> {
        self.content.resolve_pointers(pointers).await
    }
    async fn resolve_profiles(&self, addresses: &[String]) -> Result<Vec<ActiveEntity>, ApiError> {
        self.content.resolve_profiles(addresses).await
    }
    async fn resolve_one(&self, id: &str) -> Result<Option<ActiveEntity>, ApiError> {
        self.content.resolve_one(id).await
    }
    async fn resolve_candidates(
        &self,
        pointers: &[String],
        world: Option<&str>,
    ) -> Result<Vec<ActiveEntity>, ApiError> {
        self.content.resolve_candidates(pointers, world).await
    }
    async fn resolve_by_deployer(&self, deployer: &str) -> Result<Vec<ActiveEntity>, ApiError> {
        let entities = self.content.resolve_by_deployer(deployer).await?;
        let mut current = Vec::new();
        let mut historical = Vec::new();
        for entity in entities {
            if let Some(record) = self.registry.get_record(&entity.entity_id).await? {
                if matches!(record.status, dcl_contents::types::RegistryStatus::Obsolete) {
                    historical.push(entity);
                } else {
                    current.push(entity);
                }
            }
        }
        current.extend(historical);
        Ok(current)
    }
    async fn registered_status(&self, id: &str) -> Result<Option<EntityStatus>, ApiError> {
        Ok(self.registry.get_record(id).await?.map(|record| {
            dcl_contents::handlers::status::entity_status_from_bundles(
                &record.entity_id,
                &record.bundles,
            )
        }))
    }

    async fn resolve_status(
        &self,
        id: &str,
        world: Option<&str>,
    ) -> Result<Option<ActiveEntity>, ApiError> {
        let record = match self.registry.get_record(id).await? {
            Some(record) => Some(record),
            None => self
                .registry
                .records_for_pointers_all_statuses(&[id.to_string()], world)
                .await?
                .into_iter()
                .next(),
        };
        match record {
            Some(record) => self.content.resolve_one(&record.entity_id).await,
            None => Ok(None),
        }
    }
    async fn resolve_world_current(&self, world: &str) -> Result<Vec<ActiveEntity>, ApiError> {
        self.content.resolve_world_current(world).await
    }
    async fn resolve_world(&self, world: &str) -> Result<Vec<ActiveEntity>, ApiError> {
        let current = self.content.resolve_world(world).await?;
        let occupied: std::collections::HashSet<_> = current
            .iter()
            .flat_map(|entity| entity.pointers.iter().cloned())
            .collect();
        let mut ordered: Vec<_> = current.iter().collect();
        ordered.sort_by_key(|entity| entity.timestamp);
        for entity in ordered {
            self.registry
                .refresh_and_rotate(entity, &self.manifests)
                .await?;
        }
        let mut result = Vec::new();
        for record in self.registry.records_for_world(world).await? {
            // An undeployed parcel must not be revived from conversion history.
            if !record
                .pointers
                .iter()
                .any(|pointer| occupied.contains(pointer))
            {
                continue;
            }
            let entity = match current
                .iter()
                .find(|entity| entity.entity_id == record.entity_id)
            {
                Some(entity) => Some(entity.clone()),
                None => self.content.resolve_one(&record.entity_id).await?,
            };
            if let Some(mut entity) = entity {
                retain_occupied_pointers(&mut entity, &occupied);
                if !entity.pointers.is_empty() {
                    result.push(entity);
                }
            } else {
                return Err(ApiError::upstream(
                    "registered world entity is unavailable from Catalyst",
                ));
            }
        }
        Ok(result)
    }
}

// World manifests are projections, not entity documents. A converted fallback
// may cover several parcels while only some remain occupied in the live world.
fn retain_occupied_pointers(
    entity: &mut ActiveEntity,
    occupied: &std::collections::HashSet<String>,
) {
    entity.pointers.retain(|pointer| occupied.contains(pointer));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn world_fallback_projection_cannot_restore_partially_undeployed_parcels() {
        let mut old = ActiveEntity {
            deployment_id: 1,
            entity_id: "historical".into(),
            entity_type: "scene".into(),
            timestamp: 1,
            pointers: vec!["0,0".into(), "1,0".into()],
            metadata: serde_json::json!({"worldConfiguration": {"name": "world.dcl.eth"}}),
            deployer_address: None,
            content: vec![],
        };
        retain_occupied_pointers(&mut old, &HashSet::from(["0,0".into(), "2,0".into()]));
        assert_eq!(old.pointers, vec!["0,0"]);
        assert_eq!(old.entity_id, "historical");
        retain_occupied_pointers(&mut old, &HashSet::new());
        assert!(old.pointers.is_empty());
    }
}
