//! Durable queue attempts produce facts in private staging. Artifact serving is
//! still the HTTP JIT's responsibility; no queue attempt publishes local or S3
//! bytes, even if its async caller disappears while conversion is running.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use dcl_contents::types::{BuildStatus, Bundles, PlatformVersion, Versions};
use serde::Deserialize;

pub async fn convert(
    proxy: Arc<crate::live::Proxy>,
    entity: String,
    platform: String,
    content_server_url: String,
) -> Result<(Bundles, Versions)> {
    validate_platform(&platform)?;
    validate_entity(&entity)?;
    let permit = crate::abcdn::handlers::registry_conversion_permit().await?;
    run_staged(permit, move |root| {
        let isolated = proxy.isolated_registry_attempt(&root.join("cache"));
        // The guard joins background image downloads on success, error, or
        // unwinding, before run_staged removes their writable directory.
        let _prefetch = PrefetchGuard(isolated.clone());
        let corpus = root.join("corpus");
        isolated.build_entity_into_corpus(&corpus, &entity, &platform, &content_server_url)?;
        read_facts(&corpus, &entity, &platform)
    })
    .await
}

struct PrefetchGuard(Arc<crate::live::Proxy>);

impl Drop for PrefetchGuard {
    fn drop(&mut self) {
        self.0.join_registry_prefetch();
    }
}

struct Stage(PathBuf);

impl Stage {
    fn create() -> Result<Self> {
        // create_dir is exclusive: a leftover directory from an earlier process
        // must never be reused as a new attempt's cache or manifest source.
        let prefix = std::env::temp_dir().join("abgen-registry-attempt");
        loop {
            let path = crate::tmppath::tmp_sibling(&prefix);
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).context("create private Registry conversion stage")
                }
            }
        }
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            tracing::warn!(%error, path = %self.0.display(), "remove private Registry conversion stage");
        }
    }
}

async fn run_staged<T, F>(permit: tokio::sync::SemaphorePermit<'static>, build: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Path) -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        // Both owners live in the blocking task, not its cancelable awaiter.
        let _permit = permit;
        let stage = Stage::create()?;
        build(&stage.0)
    })
    .await
    .context("private Registry conversion task failed")?
}

fn validate_platform(platform: &str) -> Result<()> {
    if !matches!(platform, "windows" | "mac" | "webgl" | "linux") {
        bail!("unsupported Registry conversion platform: {platform}");
    }
    Ok(())
}

fn validate_entity(entity: &str) -> Result<()> {
    if entity.is_empty() || matches!(entity, "." | "..") || entity.contains(['/', '\\', '\0']) {
        bail!("Registry conversion entity must be one path component");
    }
    Ok(())
}

#[derive(Deserialize)]
struct AttemptManifest {
    #[serde(rename = "exitCode")]
    exit_code: i32,
    version: String,
    date: String,
}

fn read_facts(corpus: &Path, entity: &str, platform: &str) -> Result<(Bundles, Versions)> {
    validate_platform(platform)?;
    let path = corpus
        .join(&*crate::naming::fs_safe_component(entity))
        .join(format!("{platform}.manifest.json"));
    let manifest: AttemptManifest = serde_json::from_slice(
        &std::fs::read(&path)
            .with_context(|| format!("read attempted manifest {}", path.display()))?,
    )
    .context("parse attempted Registry conversion manifest")?;
    let mut bundles = Bundles::default();
    let mut versions = Versions::default();
    let (status, version) = match platform {
        "windows" => (&mut bundles.assets.windows, &mut versions.assets.windows),
        "mac" => (&mut bundles.assets.mac, &mut versions.assets.mac),
        "webgl" => (&mut bundles.assets.webgl, &mut versions.assets.webgl),
        "linux" => (&mut bundles.assets.linux, &mut versions.assets.linux),
        _ => unreachable!("validated Registry platform"),
    };
    *status = if manifest.exit_code == 0 {
        BuildStatus::Complete
    } else {
        BuildStatus::Failed
    };
    *version = PlatformVersion {
        version: manifest.version,
        build_date: manifest.date,
    };
    Ok((bundles, versions))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_attempt_manifest_supplies_status_and_versions() {
        for invalid in [
            "",
            ".",
            "..",
            "../entity",
            "/entity",
            "entity\\other",
            "entity\0",
        ] {
            assert!(validate_entity(invalid).is_err());
        }
        let stage = Stage::create().unwrap();
        let entity = "bafkattempt";
        let dir = stage.0.join(entity);
        std::fs::create_dir(&dir).unwrap();
        // Another platform's successful file cannot make this attempt succeed.
        std::fs::write(
            dir.join("mac.manifest.json"),
            r#"{"exitCode":0,"version":"v99","date":"old"}"#,
        )
        .unwrap();
        assert!(read_facts(&stage.0, entity, "windows").is_err());
        for platform in ["windows", "mac", "webgl", "linux"] {
            for (exit_code, expected) in [(0, "complete"), (12, "failed")] {
                std::fs::write(dir.join(format!("{platform}.manifest.json")),
                    serde_json::to_vec(&serde_json::json!({"exitCode":exit_code,"version":"v41","date":"build-date"})).unwrap()).unwrap();
                let (bundles, versions) = read_facts(&stage.0, entity, platform).unwrap();
                let bundles = serde_json::to_value(bundles).unwrap();
                let versions = serde_json::to_value(versions).unwrap();
                assert!(bundles.get("lods").is_none());
                for other in ["windows", "mac", "webgl", "linux"] {
                    assert_eq!(
                        bundles["assets"][other],
                        if other == platform {
                            expected
                        } else {
                            "pending"
                        }
                    );
                    assert_eq!(
                        versions["assets"][other]["version"],
                        if other == platform { "v41" } else { "" }
                    );
                    assert_eq!(
                        versions["assets"][other]["buildDate"],
                        if other == platform { "build-date" } else { "" }
                    );
                }
            }
        }
        std::fs::write(
            dir.join("windows.manifest.json"),
            r#"{"version":"v41","date":"today"}"#,
        )
        .unwrap();
        assert!(
            read_facts(&stage.0, entity, "windows").is_err(),
            "missing exitCode is not success"
        );
        assert!(read_facts(&stage.0, entity, "../windows").is_err());
    }

    #[tokio::test]
    async fn cancellation_keeps_stage_and_capacity_until_blocking_work_ends() {
        static CAPACITY: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let permit = CAPACITY.acquire().await.unwrap();
        let task = tokio::spawn(run_staged(permit, move |root| {
            started_tx.send(root.to_path_buf()).unwrap();
            release_rx.recv().unwrap();
            std::fs::write(root.join("still-owned-by-converter"), b"private")?;
            Ok(())
        }));
        let stage = started_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            stage.is_dir(),
            "canceling the awaiter must not remove live staging"
        );
        assert!(
            CAPACITY.try_acquire().is_err(),
            "blocking work still owns capacity"
        );
        release_tx.send(()).unwrap();
        let _permit = tokio::time::timeout(std::time::Duration::from_secs(5), CAPACITY.acquire())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !stage.exists(),
            "staging is removed before capacity is released"
        );
    }

    #[tokio::test]
    async fn errors_also_remove_private_stage() {
        static CAPACITY: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let result: Result<()> = run_staged(CAPACITY.acquire().await.unwrap(), move |root| {
            tx.send(root.to_path_buf()).unwrap();
            bail!("conversion failed")
        })
        .await;
        assert!(result.is_err());
        assert!(!rx.await.unwrap().exists());
    }

    #[tokio::test]
    async fn private_conversion_leaves_served_files_and_space_untouched() {
        let served = Stage::create().unwrap();
        let cache = served.0.join("cache");
        let sentinel = served.0.join("served-manifest");
        std::fs::write(&sentinel, b"existing served bytes").unwrap();
        let (space_host, space_seen, space_store) = crate::live::stub::serve_store(vec![]);
        let scene = serde_json::json!({"id":"bafkempty","type":"scene","content":[]});
        let bad_font_scene = serde_json::json!({"id":"bafkbadfont","type":"scene","content":[{"file":"bad.ttf","hash":"badfont"}]});
        let (source_host, _) = crate::live::stub::serve(vec![
            (
                "/contents/bafkempty".into(),
                200,
                serde_json::to_vec(&scene).unwrap(),
            ),
            (
                "/contents/bafkbadfont".into(),
                200,
                serde_json::to_vec(&bad_font_scene).unwrap(),
            ),
            ("/contents/badfont".into(), 200, b"not a font".to_vec()),
        ]);
        let proxy = crate::live::stub::stub_proxy_at(
            &space_host,
            &format!("http://{source_host}"),
            false,
            &cache,
        );
        let (_, bundle_root) = proxy.cache_roots();
        assert_eq!(std::fs::read_dir(bundle_root).unwrap().count(), 0);
        let (complete, _) = convert(
            proxy.clone(),
            "bafkempty".into(),
            "windows".into(),
            "http://content".into(),
        )
        .await
        .unwrap();
        assert_eq!(complete.assets.windows, BuildStatus::Complete);
        let (failed, _) = convert(
            proxy.clone(),
            "bafkbadfont".into(),
            "mac".into(),
            "http://content".into(),
        )
        .await
        .unwrap();
        assert_eq!(
            failed.assets.mac,
            BuildStatus::Failed,
            "a returned manifest with exitCode 12 is failed"
        );
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"existing served bytes");
        assert_eq!(std::fs::read_dir(bundle_root).unwrap().count(), 0);
        assert!(
            !proxy.cache_roots().0.exists(),
            "live content cache was not populated"
        );
        assert_eq!(
            std::fs::read_dir(&cache).unwrap().count(),
            1,
            "no live content/digest stores added"
        );
        assert!(
            space_seen.lock().unwrap().is_empty(),
            "private attempts make no Space probes or PUTs"
        );
        assert!(space_store.lock().unwrap().is_empty());
    }
}
