use crate::builder::{build_bundle, BuildOpts};
use crate::catalyst::{CatalystClient, Scene};
use crate::glbscan::{scan_entity, EntityScan, UriCache};
use crate::local_store::LocalContentStore;
use crate::naming;
use crate::space::Space;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

const CONVERTIBLE_EXTS: [&str; 5] = [".glb", ".gltf", ".png", ".jpg", ".jpeg"];

const DEPENDENCY_EXTS: [&str; 1] = [".bin"];

struct BuildTelemetry<'a> {
    entity: &'a str,
    entity_type: &'a str,
    file: &'a str,
    platform: &'a str,
    hash: &'a str,
    ms: u64,
    out_bytes: usize,

    result: &'a str,
}

fn emit_build_telemetry(t: &BuildTelemetry) {
    let rec = serde_json::json!({
        "entity": t.entity,
        "entity_type": t.entity_type,
        "file": t.file,
        "platform": t.platform,
        "hash": t.hash,
        "build_ms": t.ms,
        "out_bytes": t.out_bytes,
        "result": t.result,
    });
    eprintln!("ABGEN_BUILD {rec}");
}

fn is_convertible(file: &str) -> (bool, bool) {
    let fl = file.to_lowercase();
    let is_glb = fl.ends_with(".glb") || fl.ends_with(".gltf");
    let is_image = fl.ends_with(".png") || fl.ends_with(".jpg") || fl.ends_with(".jpeg");
    (is_glb, is_image)
}

/// Entity files a conversion actually emits bundles for: convertible by
/// extension, minus the hashes upstream drops via `-skippedHashes`. Skipped
/// files leave no manifest entry and no failure, matching the prod converter's
/// output shape (an attempt that "merely" tolerates the failure would report
/// exitCode 12 and carry extra names).
fn convertible_entries<'a>(
    scene: &'a Scene,
    skipped_hashes: &std::collections::HashSet<String>,
) -> Vec<&'a crate::catalyst::ContentEntry> {
    scene
        .content
        .iter()
        .filter(|c| {
            let lf = c.file.to_lowercase();
            CONVERTIBLE_EXTS.iter().any(|e| lf.ends_with(e)) && !skipped_hashes.contains(&c.hash)
        })
        .collect()
}

fn fetch_jobs() -> usize {
    static JOBS: OnceLock<usize> = OnceLock::new();
    *JOBS.get_or_init(|| {
        match std::env::var("ABGEN_FETCH_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
        {
            Some(n) if n > 0 => n,
            _ => 8,
        }
    })
}

fn corpus_file_jobs() -> usize {
    static JOBS: OnceLock<usize> = OnceLock::new();
    *JOBS.get_or_init(|| {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        match std::env::var("ABGEN_JIT_FILE_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
        {
            Some(n) if n > 0 => n,
            _ => cores.min(4),
        }
    })
}

/// Deps digests for every distinct convertible GLB in `items` — (hash, file)
/// pairs in entity content order, duplicates and non-GLBs welcome; the first
/// occurrence of a hash picks the file name, exactly like the serial scan it
/// replaces. Each digest parses its whole GLB, so the work is spread across
/// `jobs` workers. Returns the digest map plus warnings as (file, hash,
/// rendered error), in work order.
fn compute_deps_digests(
    content: &LocalContentStore,
    items: &[(String, String)],
    content_by_file: &HashMap<String, String>,
    tolerant: bool,
    jobs: usize,
) -> (
    HashMap<String, String>,
    std::collections::HashSet<String>,
    Vec<(String, String, String)>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let work: Vec<&(String, String)> = items
        .iter()
        .filter(|(hash, file)| is_convertible(file).0 && seen.insert(hash.clone()))
        .collect();

    let slots: Vec<Mutex<Option<Result<String, (bool, String)>>>> =
        work.iter().map(|_| Mutex::new(None)).collect();
    let workers = jobs.clamp(1, work.len().max(1));
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((hash, file)) = work.get(i).map(|w| (&w.0, &w.1)) else {
                    break;
                };
                let digest = content.fetch_mmap(hash).and_then(|bytes| {
                    naming::deps_digest_for_glb(&bytes, file, content_by_file, tolerant)
                });
                *slots[i].lock().unwrap() = Some(digest.map_err(|e| {
                    let undeployed = e.downcast_ref::<naming::DepNotDeployed>().is_some();
                    (undeployed, format!("{e:#}"))
                }));
            });
        }
    });

    let mut digests: HashMap<String, String> = HashMap::new();
    let mut undeployed: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut warns: Vec<(String, String, String)> = Vec::new();
    for ((hash, file), slot) in work.iter().map(|w| (&w.0, &w.1)).zip(slots) {
        match slot.into_inner().unwrap() {
            Some(Ok(d)) => {
                digests.insert(hash.clone(), d);
            }
            Some(Err((true, e))) => {
                undeployed.insert(hash.clone());
                warns.push((file.clone(), hash.clone(), e));
            }
            Some(Err((false, e))) => warns.push((file.clone(), hash.clone(), e)),
            None => {}
        }
    }
    (digests, undeployed, warns)
}

fn bounded_reserve<V>(map: &mut HashMap<String, V>, cap: usize, key: &str) {
    if map.len() >= cap && !map.contains_key(key) {
        if let Some(k) = map.keys().next().cloned() {
            map.remove(&k);
        }
    }
}

#[derive(Default)]
struct KeyedLocks {
    map: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl KeyedLocks {
    fn get(&self, key: &str) -> Arc<Mutex<()>> {
        let mut g = self.map.lock().unwrap();
        g.entry(key.to_string()).or_default().clone()
    }
}

pub(crate) struct EntityCtx {
    pub(crate) scene: Scene,
    pub(crate) content_by_file: HashMap<String, String>,
    pub(crate) scan: EntityScan,

    deps_digests: HashMap<String, String>,
    /// GLBs whose referenced textures are not deployed in the entity: prod
    /// skips these (no manifest entry, exit 0), so they must not count as
    /// conversion failures (#59).
    undeployed_dep_glbs: std::collections::HashSet<String>,
}

pub struct Proxy {
    catalyst: CatalystClient,
    local: Option<LocalContentStore>,
    content: LocalContentStore,
    bundle_dir: PathBuf,
    digests_dir: PathBuf,
    version: String,
    date: String,
    uri_cache: UriCache,

    space: Option<Arc<Space>>,
    fallback_version: String,

    entities: Mutex<HashMap<String, Arc<EntityCtx>>>,
    hash_index: Mutex<HashMap<String, String>>,
    entity_cap: usize,
    hash_index_cap: usize,
    entity_locks: KeyedLocks,
    bundle_locks: KeyedLocks,
    collection_mode: bool,
    real_textures: bool,
    v38_compat: bool,
    v38_timestamp: i64,
    magenta_missing: bool,
    jit_cache: OnceLock<Arc<crate::jitcache::JitDiskCache>>,
    asset_reuse: bool,
    build_progress: Mutex<HashMap<String, BuildProgress>>,
    /// Content is immutable per hash, so a standalone image's decode verdict
    /// holds for every platform and repeat conversion in this process.
    decode_ok: Mutex<HashMap<String, bool>>,
}

#[derive(Clone)]
pub struct BuildProgress {
    pub done: usize,
    pub total: usize,
    pub file: String,
}

struct ProgressGuard<'a> {
    proxy: &'a Proxy,
    cid: &'a str,
}

impl Drop for ProgressGuard<'_> {
    fn drop(&mut self) {
        self.proxy.progress_clear(self.cid);
    }
}

impl Proxy {
    fn jit(&self) -> Option<&Arc<crate::jitcache::JitDiskCache>> {
        self.jit_cache.get().filter(|c| c.enabled())
    }

    pub fn set_jit_cache(&self, c: Arc<crate::jitcache::JitDiskCache>) {
        let _ = self.jit_cache.set(c);
    }

    pub fn progress_snapshot(&self, cid: &str) -> Option<BuildProgress> {
        self.build_progress.lock().ok()?.get(cid).cloned()
    }

    fn progress_update(&self, cid: &str, done: usize, total: usize, file: &str) {
        if let Ok(mut g) = self.build_progress.lock() {
            g.insert(
                cid.to_string(),
                BuildProgress {
                    done,
                    total,
                    file: file.to_string(),
                },
            );
        }
    }

    fn progress_clear(&self, cid: &str) {
        if let Ok(mut g) = self.build_progress.lock() {
            g.remove(cid);
        }
    }

    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn cache_roots(&self) -> (&Path, &Path) {
        (self.content.root(), self.bundle_dir.as_path())
    }

    fn record_content(&self, hash: &str, len: usize) {
        if len == 0 {
            return;
        }
        if let Some(c) = self.jit() {
            c.record(&format!("c:{hash}"), self.content.path_of(hash), len as u64);
        }
    }

    pub(crate) fn content_store(&self) -> &LocalContentStore {
        &self.content
    }

    pub(crate) fn ensure_content(&self, hash: &str) -> Result<()> {
        if self.content.exists(hash) {
            if let Some(c) = self.jit() {
                c.touch(&format!("c:{hash}"));
            }
            return Ok(());
        }
        if let Some(local) = &self.local {
            if let Ok(b) = local.fetch(hash) {
                if !b.is_empty() {
                    self.content.write(hash, &b)?;
                    self.record_content(hash, b.len());
                    return Ok(());
                }
            }
        }
        let bytes = self
            .catalyst
            .fetch_content(hash)
            .with_context(|| format!("fetch content {hash}"))?;
        if bytes.is_empty() {
            bail!("fetch content {hash}: empty payload");
        }
        self.content.write(hash, &bytes)?;
        self.record_content(hash, bytes.len());
        Ok(())
    }

    pub(crate) fn content_bytes_allow_empty(&self, hash: &str) -> Result<Vec<u8>> {
        match self.ensure_content(hash) {
            Ok(()) => self.content_store().fetch(hash),
            Err(e) => {
                let bytes = self.catalyst.fetch_content(hash)?;
                if bytes.is_empty() {
                    return Ok(bytes);
                }
                Err(e)
            }
        }
    }

    pub(crate) fn entity_ctx(&self, cid: &str) -> Result<Arc<EntityCtx>> {
        if let Some(c) = self.entities.lock().unwrap().get(cid) {
            return Ok(c.clone());
        }
        let lock = self.entity_locks.get(cid);
        let _g = lock.lock().unwrap();
        if let Some(c) = self.entities.lock().unwrap().get(cid) {
            return Ok(c.clone());
        }

        let scene = self
            .catalyst
            .resolve_scene(cid)
            .with_context(|| format!("resolve entity {cid}"))?;

        let dl_started = std::time::Instant::now();
        let mut to_fetch: Vec<(&str, &str)> = Vec::new();
        let mut seen_hashes = std::collections::HashSet::new();
        for c in &scene.content {
            if CONVERTIBLE_EXTS
                .iter()
                .chain(DEPENDENCY_EXTS.iter())
                .any(|e| c.file.to_lowercase().ends_with(*e))
                && seen_hashes.insert(c.hash.as_str())
            {
                to_fetch.push((c.hash.as_str(), c.file.as_str()));
            }
        }
        let dl_files = to_fetch.len();
        let workers = fetch_jobs().min(dl_files.max(1));
        let next_fetch = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let i = next_fetch.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((hash, file)) = to_fetch.get(i) else {
                        break;
                    };
                    if let Err(e) = self.ensure_content(hash) {
                        eprintln!("warn: {cid}: content {hash} ({file}): {e}");
                    }
                });
            }
        });
        eprintln!(
            "download: {cid}: {dl_files} file(s) ensured in {:.1}s ({workers} worker(s))",
            dl_started.elapsed().as_secs_f64()
        );

        let content_by_file = scene.content_by_file();
        let scan = scan_entity(&self.content, &content_by_file, &self.uri_cache);

        let digest_items: Vec<(String, String)> = scene
            .content
            .iter()
            .map(|c| (c.hash.clone(), c.file.clone()))
            .collect();
        let (deps_digests, undeployed_dep_glbs, digest_warns) = compute_deps_digests(
            &self.content,
            &digest_items,
            &content_by_file,
            self.magenta_missing,
            corpus_file_jobs(),
        );
        for (file, hash, e) in &digest_warns {
            eprintln!("warn: {cid}: deps digest for {file} ({hash}): {e}");
        }

        {
            let mut idx = self.hash_index.lock().unwrap();
            for c in &scene.content {
                let key = c.hash.to_lowercase();
                bounded_reserve(&mut idx, self.hash_index_cap, &key);
                idx.entry(key).or_insert_with(|| cid.to_string());
            }
        }

        let ctx = Arc::new(EntityCtx {
            scene,
            content_by_file,
            scan,
            deps_digests,
            undeployed_dep_glbs,
        });
        let mut g = self.entities.lock().unwrap();
        bounded_reserve(&mut g, self.entity_cap, cid);
        g.insert(cid.to_string(), ctx.clone());
        Ok(ctx)
    }

    /// Whether the standalone image behind `hash` decodes. The verdict feeds
    /// the manifest's tolerated count only; it is memoized per hash so the
    /// full decode runs once per content, not once per platform pass.
    fn image_decode_ok(&self, hash: &str) -> bool {
        if let Some(v) = self.decode_ok.lock().unwrap().get(hash) {
            return *v;
        }
        self.ensure_content(hash).ok();
        let Ok(raw) = self.content.fetch_mmap(hash) else {
            // Unreadable content is not a decode failure (the old inline
            // check skipped it too); left un-memoized so a later successful
            // fetch still gets a real verdict.
            return true;
        };
        let v = crate::builder::source_image_decodes(&raw);
        self.decode_ok.lock().unwrap().insert(hash.to_string(), v);
        v
    }

    fn bundle(&self, cid: &str, bundle_name: &str) -> Result<Vec<u8>> {
        let safe_cid = naming::fs_safe_component(cid);
        let entity_dir = self.bundle_dir.join(&*safe_cid);
        let cache_path = entity_dir.join(&*naming::fs_safe_component(bundle_name));
        if let Ok(b) = std::fs::read(&cache_path) {
            if let Some(c) = self.jit() {
                c.touch(&format!("b:{safe_cid}"));
            }
            return Ok(b);
        }
        let lock = self.bundle_locks.get(&format!("{cid}/{bundle_name}"));
        let _g = lock.lock().unwrap();
        if let Ok(b) = std::fs::read(&cache_path) {
            if let Some(c) = self.jit() {
                c.touch(&format!("b:{safe_cid}"));
            }
            return Ok(b);
        }

        let _pin = self.jit().and_then(|c| c.pin(&format!("b:{safe_cid}")));

        let ctx = self.entity_ctx(cid)?;
        let data = self.build(&ctx, bundle_name)?;

        std::fs::create_dir_all(&entity_dir).ok();
        let tmp = crate::tmppath::tmp_sibling(&cache_path);
        std::fs::write(&tmp, &data).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &cache_path).ok();
        if let Some(c) = self.jit() {
            let bytes = crate::jitcache::dir_size(&entity_dir);
            if bytes > 0 {
                c.record(&format!("b:{safe_cid}"), entity_dir.clone(), bytes);
            }
        }
        Ok(data)
    }

    fn build(&self, ctx: &EntityCtx, bundle_name: &str) -> Result<Vec<u8>> {
        let (stem, platform) = bundle_name
            .rsplit_once('_')
            .ok_or_else(|| anyhow!("bundle name {bundle_name:?} has no _<platform> suffix"))?;
        let (hash, req_digest) = naming::split_bundle_stem(stem);

        let item = match ctx
            .scene
            .content
            .iter()
            .find(|c| {
                if !c.hash.eq_ignore_ascii_case(hash) {
                    return false;
                }
                let (g, i) = is_convertible(&c.file);
                g || i
            })
            .or_else(|| {
                ctx.scene
                    .content
                    .iter()
                    .find(|c| c.hash.eq_ignore_ascii_case(hash))
            }) {
            Some(it) => it,
            None => {
                if let Some(owner) = self.entity_for_hash(hash) {
                    if !owner.eq_ignore_ascii_case(&ctx.scene.entity_id) {
                        let owner_ctx = self.entity_ctx(&owner)?;
                        return self.build(&owner_ctx, bundle_name);
                    }
                }
                bail!(
                    "hash {hash} not in entity {} (no owning entity indexed)",
                    ctx.scene.entity_id
                );
            }
        };
        let hash: &str = &item.hash;
        let file = item.file.clone();
        let (is_glb, is_image) = is_convertible(&file);
        if !is_glb && !is_image {
            bail!("content {file} (hash {hash}) is not a convertible glb/image");
        }
        if let Some(req_digest) = req_digest {
            if !is_glb {
                bail!(
                    "bundle name {bundle_name:?} carries a deps digest but {file} is not glb/gltf"
                );
            }
            match ctx.deps_digests.get(hash) {
                Some(d) if d == req_digest => {}
                Some(d) => bail!(
                    "deps digest mismatch for {file} (hash {hash}): requested {req_digest}, computed {d}"
                ),
                None => bail!(
                    "deps digest unavailable for {file} (hash {hash}): dependency resolution failed at entity scan"
                ),
            }
        }

        self.ensure_content(hash)?;
        let glb = match self.content.fetch(hash) {
            Ok(b) => b,
            Err(_) => {
                self.ensure_content(hash)?;
                self.content.fetch(hash)?
            }
        };

        let m_deps = if is_glb {
            ctx.scan
                .metadata_deps(&self.content, &file, hash, &ctx.content_by_file, platform)
        } else {
            Vec::new()
        };
        let model_ref = is_image && ctx.scan.model_refs.contains(hash);
        let standalone_color_space = if is_image {
            Some(if ctx.scan.linear_refs.contains(hash) {
                0
            } else {
                1
            })
        } else {
            None
        };
        let standalone_normal = is_image && ctx.scan.normal_refs.contains(hash);

        let content_by_file = &ctx.content_by_file;
        let resolve_fn = |uri: &str| -> Option<Vec<u8>> {
            let h = naming::uri_content_hash(uri, &file, content_by_file)?;
            if let Err(e) = self.ensure_content(h) {
                eprintln!("warn: resolve {uri} (hash {h}): {e:#}");
            }
            self.content.fetch(h).ok().or_else(|| {
                let _ = self.ensure_content(h);
                self.content.fetch(h).ok()
            })
        };
        let resolve: crate::gltf::Resolve = if !content_by_file.is_empty() {
            Some(&resolve_fn)
        } else {
            None
        };
        let resolve_hash_fn = |uri: &str| -> Option<String> {
            naming::uri_content_hash(uri, &file, content_by_file).cloned()
        };
        let resolve_hash: Option<crate::builder::ResolveHash> = if !content_by_file.is_empty() {
            Some(&resolve_hash_fn)
        } else {
            None
        };

        let entity_type = ctx.scene.entity_type.clone();
        let opts = BuildOpts {
            keep_forward_plus: true,
            source_file: Some(&file),
            entity_type: if entity_type.is_empty() {
                None
            } else {
                Some(entity_type.as_str())
            },
            resolve,
            resolve_hash,
            model_referenced: model_ref,
            metadata_dependencies: &m_deps,
            expect_hash: None,
            standalone_color_space,
            standalone_normal,
            force_default_material: false,
            magenta_missing: self.magenta_missing,
            collection_mode: self.collection_mode,
            real_textures: self.real_textures,
            v38_compat: self.v38_compat,
            v38_timestamp: self.v38_timestamp,
            lod: None,
        };

        let started = std::time::Instant::now();
        let outcome = crate::regen::guard(|| build_bundle(&glb, bundle_name, hash, &opts));
        let ms = started.elapsed().as_millis() as u64;

        let (result_label, out_bytes) = match &outcome {
            Ok(a) => ("ok", a.data.len()),
            Err(e) => {
                if e.to_string().starts_with("panic:") {
                    ("panic-recovered", 0usize)
                } else {
                    ("error", 0usize)
                }
            }
        };
        emit_build_telemetry(&BuildTelemetry {
            entity: &ctx.scene.entity_id,
            entity_type: &entity_type,
            file: &file,
            platform,
            hash,
            ms,
            out_bytes,
            result: result_label,
        });

        let artifact = outcome?;
        Ok(artifact.data)
    }

    pub fn entity_for_hash(&self, hash: &str) -> Option<String> {
        self.hash_index
            .lock()
            .unwrap()
            .get(&hash.to_lowercase())
            .cloned()
    }

    pub fn index_content_hashes<I: IntoIterator<Item = (String, String)>>(&self, pairs: I) {
        let mut idx = self.hash_index.lock().unwrap();
        for (hash, entity) in pairs {
            let key = hash.to_lowercase();
            bounded_reserve(&mut idx, self.hash_index_cap, &key);
            idx.entry(key).or_insert(entity);
        }
    }

    fn bundle_key(version: &str, cid: &str, file: &str) -> String {
        format!("{version}/{cid}/{file}")
    }

    fn asset_bundle_key(version: &str, file: &str) -> String {
        format!("{version}/assets/{file}")
    }

    pub fn space_configured(&self) -> bool {
        self.space.is_some()
    }

    pub fn space_bucket(&self) -> Option<&str> {
        self.space.as_ref()?.bucket.as_deref()
    }

    /// Bucket-scoped: a hit in one CDN bucket says nothing about another.
    fn reuse_cache_key(&self, key: &str) -> Option<String> {
        if !crate::rediscache::enabled() {
            return None;
        }
        let bucket = self.space.as_ref()?.bucket.as_deref().unwrap_or("-");
        Some(format!("abgen:hit:{bucket}:{key}"))
    }

    fn space_get_timed(space: &crate::space::Space, key: &str) -> crate::Result<Option<Vec<u8>>> {
        let t = std::time::Instant::now();
        let r = space.get(key);
        let result = match &r {
            Ok(Some(_)) => "hit",
            Ok(None) => "miss",
            Err(_) => "error",
        };
        metrics::histogram!("abgen_space_request_duration_seconds", "op" => "get", "result" => result)
            .record(t.elapsed().as_secs_f64());
        if let Ok(Some(b)) = &r {
            metrics::counter!("abgen_space_transfer_bytes_total", "direction" => "download")
                .increment(b.len() as u64);
            tracing::info!(key = %key, bytes = b.len(), ms = t.elapsed().as_millis() as u64, "space hit");
        }
        if r.is_err() {
            metrics::counter!("abgen_space_errors_total", "op" => "get").increment(1);
        }
        r
    }

    fn space_put_timed(space: &crate::space::Space, key: &str, bytes: &[u8]) -> crate::Result<()> {
        let t = std::time::Instant::now();
        let r = space.put(key, bytes);
        let result = if r.is_ok() { "ok" } else { "error" };
        metrics::histogram!("abgen_space_request_duration_seconds", "op" => "put", "result" => result)
            .record(t.elapsed().as_secs_f64());
        if r.is_ok() {
            metrics::counter!("abgen_space_transfer_bytes_total", "direction" => "upload")
                .increment(bytes.len() as u64);
            metrics::histogram!("abgen_space_object_bytes").record(bytes.len() as f64);
        } else {
            metrics::counter!("abgen_space_errors_total", "op" => "put").increment(1);
        }
        r
    }

    pub fn space_get_bundle(&self, cid: &str, file: &str) -> Option<Vec<u8>> {
        let space = self.space.as_ref()?;
        let mut versions = vec![self.version.as_str()];
        if self.fallback_version != self.version {
            versions.push(self.fallback_version.as_str());
        }
        for ver in versions {
            let mut keys: Vec<String> = Vec::new();
            if self.asset_reuse && naming::bundle_name_has_digest(file) {
                keys.push(Self::asset_bundle_key(ver, file));
            }
            keys.push(Self::bundle_key(ver, cid, file));
            for key in keys {
                match Self::space_get_timed(space, &key) {
                    Ok(Some(b)) => return Some(b),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(key = %key, error = %format!("{e:#}"), "space get failed; trying next key");
                    }
                }
            }
        }
        None
    }

    fn space_probe_asset(&self, file: &str) -> bool {
        if !self.asset_reuse || !naming::bundle_name_has_digest(file) {
            return false;
        }
        let Some(space) = self.space.as_ref() else {
            return false;
        };
        let key = Self::asset_bundle_key(&self.version, file);
        let cache_key = self.reuse_cache_key(&key);
        if let Some(ck) = &cache_key {
            if crate::rediscache::hit(ck) {
                return true;
            }
        }
        let t = std::time::Instant::now();
        let r = space.head(&key);
        let result = match &r {
            Ok(true) => "hit",
            Ok(false) => "miss",
            Err(_) => "error",
        };
        metrics::histogram!("abgen_space_request_duration_seconds", "op" => "head", "result" => result)
            .record(t.elapsed().as_secs_f64());
        match r {
            Ok(hit) => {
                if hit {
                    if let Some(ck) = &cache_key {
                        crate::rediscache::mark(ck);
                    }
                }
                hit
            }
            Err(e) => {
                metrics::counter!("abgen_space_errors_total", "op" => "head").increment(1);
                tracing::warn!(key = %key, error = %format!("{e:#}"), "space probe failed; building locally");
                false
            }
        }
    }

    pub fn space_get_key(&self, key: &str) -> Option<Vec<u8>> {
        let space = self.space.as_ref()?;
        match Self::space_get_timed(space, key) {
            Ok(Some(b)) => Some(b),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(key = %key, error = %format!("{e:#}"), "space get failed");
                None
            }
        }
    }

    pub fn space_put_key(&self, key: &str, bytes: &[u8]) {
        let Some(space) = self.space.as_ref() else {
            return;
        };
        if space.read_only {
            return;
        }
        match Self::space_put_timed(space, key, bytes) {
            Ok(()) => tracing::info!(key = %key, bytes = bytes.len(), "space put"),
            Err(e) => tracing::warn!(key = %key, error = %format!("{e:#}"), "space put failed"),
        }
    }

    pub fn space_probe_versions(&self, first: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for v in [first, self.version.as_str(), self.fallback_version.as_str()] {
            if !v.is_empty() && !out.iter().any(|o| o == v) {
                out.push(v.to_string());
            }
        }
        out
    }

    pub fn space_get_manifest(&self, stem: &str) -> Option<Vec<u8>> {
        self.space_get_key(&format!("manifest/{stem}.json"))
    }

    pub fn space_put_bundle(&self, cid: &str, file: &str, bytes: &[u8]) {
        let key = if self.asset_reuse && naming::bundle_name_has_digest(file) {
            Self::asset_bundle_key(&self.version, file)
        } else {
            Self::bundle_key(&self.version, cid, file)
        };
        self.space_put_key(&key, bytes);
    }

    pub fn space_put_manifest(&self, stem: &str, bytes: &[u8]) {
        self.space_put_key(&format!("manifest/{stem}.json"), bytes);
    }

    pub fn date(&self) -> &str {
        &self.date
    }

    pub fn build_entity_into_corpus(
        self: &Arc<Self>,
        out_root: &std::path::Path,
        cid: &str,
        platform: &str,
        content_server_url: &str,
    ) -> Result<Vec<String>> {
        if platform == crate::bvwebgpu::BVW_PLATFORM {
            self.build_bvwebgpu_pack(out_root, cid)?;
            return Ok(vec![crate::bvwebgpu::pack_file_name(cid)]);
        }
        let ctx = self.entity_ctx(cid)?;
        let pdir = out_root
            .join(&*naming::fs_safe_component(cid))
            .join(platform);
        std::fs::create_dir_all(&pdir).with_context(|| format!("mkdir {}", pdir.display()))?;
        let mut failed: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut collapsed_names: HashMap<String, String> = HashMap::new();
        let skipped = ctx.scene.metadata_only_hashes();
        let convertible = convertible_entries(&ctx.scene, &skipped);
        if !skipped.is_empty() {
            tracing::info!(
                entity = %cid,
                count = skipped.len(),
                "metadata-only files dropped from conversion input"
            );
        }
        let total = convertible.len();
        let _progress = ProgressGuard { proxy: self, cid };

        struct WorkItem {
            order: usize,
            file: String,
            hash: String,
            bundle_name: String,
            bare_name: String,
            is_image: bool,
        }
        let mut prebuilt: Vec<(usize, String)> = Vec::new();
        let mut work: Vec<WorkItem> = Vec::new();
        let mut order: usize = 0;
        let mut done_pre: usize = 0;
        for c in &convertible {
            order += 1;
            let (is_glb, is_image) = is_convertible(&c.file);
            let case_hash = if platform == "mac" {
                c.hash.to_lowercase()
            } else {
                c.hash.clone()
            };
            let bare_name = format!("{case_hash}_{platform}");
            let digest_naming = self.asset_reuse && is_glb && ctx.scene.entity_type == "scene";
            let bundle_name = if digest_naming {
                match ctx.deps_digests.get(&c.hash) {
                    Some(d) => format!("{case_hash}_{d}_{platform}"),
                    None if ctx.undeployed_dep_glbs.contains(&c.hash) => {
                        // Broken deployment, not a conversion error: prod
                        // skips these GLBs entirely (no manifest entry,
                        // exit 0) — counting them as failed broke exit-code
                        // parity on every scene carrying one (#59).
                        tracing::warn!(
                            entity = %cid,
                            file = %c.file,
                            hash = %c.hash,
                            "glb references undeployed textures — skipped (prod parity)"
                        );
                        done_pre += 1;
                        continue;
                    }
                    None => {
                        tracing::error!(
                            entity = %cid,
                            file = %c.file,
                            hash = %c.hash,
                            "no deps digest — glb omitted from manifest, exitCode will be non-zero"
                        );
                        failed.push(bare_name);
                        done_pre += 1;
                        continue;
                    }
                }
            } else {
                bare_name.clone()
            };
            if !seen.insert(bundle_name.clone()) {
                done_pre += 1;
                continue;
            }
            if self.space_probe_asset(&bundle_name) {
                prebuilt.push((order, bundle_name));
                done_pre += 1;
                continue;
            }
            let stored_name = naming::fs_safe_component(&bundle_name);
            if *stored_name != bundle_name {
                collapsed_names.insert(stored_name.to_string(), c.hash.to_lowercase());
            }
            work.push(WorkItem {
                order,
                file: c.file.clone(),
                hash: c.hash.clone(),
                bundle_name,
                bare_name,
                is_image,
            });
        }

        use std::sync::atomic::{AtomicUsize, Ordering};
        let jobs = corpus_file_jobs().min(work.len().max(1));
        let built_m: Mutex<Vec<(usize, String)>> = Mutex::new(prebuilt);
        let failed_m: Mutex<Vec<String>> = Mutex::new(failed);
        let collapsed_m: Mutex<HashMap<String, String>> = Mutex::new(collapsed_names);
        let tolerated_a = AtomicUsize::new(0);
        let done = AtomicUsize::new(done_pre);
        let next = AtomicUsize::new(0);
        let hard_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);

        let run_item = |it: &WorkItem| -> Result<()> {
            self.progress_update(cid, done.load(Ordering::Relaxed), total, &it.file);
            let stored_name = naming::fs_safe_component(&it.bundle_name);
            let dst = pdir.join(&*stored_name);
            let existed = dst.is_file();
            match self.bundle(cid, &it.bundle_name) {
                Ok(bytes) => {
                    let tmp = crate::tmppath::tmp_sibling(&dst);
                    std::fs::write(&tmp, &bytes)
                        .with_context(|| format!("write {}", tmp.display()))?;
                    std::fs::rename(&tmp, &dst).ok();
                    if it.bundle_name != it.bare_name {
                        let stored_alias = naming::fs_safe_component(&it.bare_name);
                        if *stored_alias != it.bare_name {
                            collapsed_m
                                .lock()
                                .unwrap()
                                .insert(stored_alias.to_string(), it.hash.to_lowercase());
                        }
                        let alias = pdir.join(&*stored_alias);
                        if !alias.exists() {
                            std::fs::hard_link(&dst, &alias).ok();
                        }
                    }
                    if !existed {
                        self.space_put_bundle(cid, &it.bundle_name, &bytes);
                    }
                    built_m
                        .lock()
                        .unwrap()
                        .push((it.order, it.bundle_name.clone()));
                    if it.is_image && !self.image_decode_ok(&it.hash) {
                        tracing::warn!(
                            entity = %cid,
                            file = %it.file,
                            hash = %it.hash,
                            "source image does not decode — bundle kept, exitCode will be non-zero"
                        );
                        tolerated_a.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    tracing::error!(
                        entity = %cid,
                        bundle = %it.bundle_name,
                        file = %it.file,
                        error = %format!("{e:#}"),
                        "jit build failed — omitted from manifest, exitCode will be non-zero"
                    );
                    failed_m.lock().unwrap().push(it.bundle_name.clone());
                }
            }
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            self.progress_update(cid, d, total, &it.file);
            Ok(())
        };

        if jobs <= 1 {
            for it in &work {
                run_item(it)?;
            }
        } else {
            std::thread::scope(|s| {
                for _ in 0..jobs {
                    s.spawn(|| loop {
                        if hard_err.lock().unwrap().is_some() {
                            break;
                        }
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= work.len() {
                            break;
                        }
                        if let Err(e) = run_item(&work[i]) {
                            let mut g = hard_err.lock().unwrap();
                            if g.is_none() {
                                *g = Some(e);
                            }
                            break;
                        }
                    });
                }
            });
            if let Some(e) = hard_err.into_inner().unwrap() {
                return Err(e);
            }
        }

        let mut built_pairs = built_m.into_inner().unwrap();
        built_pairs.sort_by_key(|p| p.0);
        let built: Vec<String> = built_pairs.into_iter().map(|p| p.1).collect();
        let failed = failed_m.into_inner().unwrap();
        let tolerated = tolerated_a.load(Ordering::Relaxed);
        let collapsed_names = collapsed_m.into_inner().unwrap();
        self.merge_names_index(cid, &collapsed_names);
        let manifest_path =
            crate::manifest::write_corpus_manifest(&crate::manifest::CorpusManifestSpec {
                out_root,
                entity_id: cid,
                platform,
                built: &built,
                ab_version: &self.version,
                content_server_url,
                exit_code: crate::manifest::exit_code_for_failures(failed.len() + tolerated),
                date: &self.date,
            })?;
        if self.space_configured() {
            match std::fs::read(&manifest_path) {
                Ok(mbytes) => self.space_put_manifest(&format!("{cid}_{platform}"), &mbytes),
                Err(e) => tracing::warn!(
                    path = %manifest_path.display(),
                    error = %e,
                    "manifest read for space put failed"
                ),
            }
        }
        Ok(built)
    }

    const VERSIONED_ID_DIGEST: &'static str = "id-versioned";

    pub fn refresh_entity_content(&self, cid: &str, corpus_root: &Path) -> Result<Vec<String>> {
        let scene = self
            .catalyst
            .resolve_scene(cid)
            .with_context(|| format!("revalidate: resolve entity {cid}"))?;

        let digest_path = self.digest_path(cid);
        let old: HashMap<String, String> = std::fs::read(&digest_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();

        let mut fresh: HashMap<String, String> = HashMap::new();
        let mut changed: Vec<String> = Vec::new();
        let mut glb_hashes: std::collections::HashSet<String> = std::collections::HashSet::new();
        for c in &scene.content {
            let lf = c.file.to_lowercase();
            if !CONVERTIBLE_EXTS
                .iter()
                .chain(DEPENDENCY_EXTS.iter())
                .any(|e| lf.ends_with(e))
            {
                continue;
            }
            if fresh.contains_key(&c.hash) {
                continue;
            }
            if naming::is_content_versioned_id(&c.hash) {
                if !old.contains_key(&c.hash) {
                    changed.push(c.hash.clone());
                }
                let (is_glb, _) = is_convertible(&c.file);
                if is_glb {
                    glb_hashes.insert(c.hash.to_lowercase());
                }
                fresh.insert(c.hash.clone(), Self::VERSIONED_ID_DIGEST.to_string());
                continue;
            }
            let bytes = match self.catalyst.fetch_content(&c.hash) {
                Ok(b) if !b.is_empty() => b,
                Ok(_) => {
                    tracing::warn!(entity = %cid, file = %c.file, hash = %c.hash, "revalidate: empty content payload — skipped");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(entity = %cid, file = %c.file, hash = %c.hash, error = %format!("{e:#}"), "revalidate: content fetch failed — skipped");
                    continue;
                }
            };
            let digest = crate::hashes::sha256_hex(&bytes);
            if old.get(&c.hash) != Some(&digest) {
                self.content
                    .write(&c.hash, &bytes)
                    .with_context(|| format!("revalidate: refresh content {}", c.hash))?;
                self.record_content(&c.hash, bytes.len());
                self.uri_cache.invalidate_hash(&c.hash);
                changed.push(c.hash.clone());
            }
            let (is_glb, _) = is_convertible(&c.file);
            if is_glb {
                glb_hashes.insert(c.hash.to_lowercase());
            }
            fresh.insert(c.hash.clone(), digest);
        }

        if let Some(dir) = digest_path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let tmp = crate::tmppath::tmp_sibling(&digest_path);
        std::fs::write(&tmp, serde_json::to_vec(&fresh)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &digest_path).ok();

        if changed.is_empty() {
            return Ok(changed);
        }

        self.entities.lock().unwrap().remove(cid);

        let changed_lower: std::collections::HashSet<String> =
            changed.iter().map(|h| h.to_lowercase()).collect();
        let dep_changed = changed_lower.iter().any(|h| !glb_hashes.contains(h));
        prune_stale_bundles(
            &self.bundle_dir.join(&*naming::fs_safe_component(cid)),
            &changed_lower,
            dep_changed,
            &glb_hashes,
            &self.load_names_index(cid),
        );
        let _ = std::fs::remove_dir_all(corpus_root.join(&*naming::fs_safe_component(cid)));
        Ok(changed)
    }

    fn digest_path(&self, cid: &str) -> PathBuf {
        let key = crate::hashes::sha256_hex(cid.as_bytes());
        self.digests_dir.join(format!("{}.json", &key[..32]))
    }

    fn names_index_path(&self, cid: &str) -> PathBuf {
        let key = crate::hashes::sha256_hex(cid.as_bytes());
        self.digests_dir.join(format!("{}.names.json", &key[..32]))
    }

    fn load_names_index(&self, cid: &str) -> HashMap<String, String> {
        std::fs::read(self.names_index_path(cid))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn merge_names_index(&self, cid: &str, new_entries: &HashMap<String, String>) {
        if new_entries.is_empty() {
            return;
        }
        let mut index = self.load_names_index(cid);
        let before = index.len();
        index.extend(new_entries.iter().map(|(k, v)| (k.clone(), v.clone())));
        if index.len() == before && new_entries.iter().all(|(k, v)| index.get(k) == Some(v)) {
            return;
        }
        let path = self.names_index_path(cid);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        if let Ok(bytes) = serde_json::to_vec(&index) {
            let tmp = crate::tmppath::tmp_sibling(&path);
            if std::fs::write(&tmp, bytes).is_ok() {
                std::fs::rename(&tmp, &path).ok();
            }
        }
    }
}

fn prune_stale_bundles(
    dir: &Path,
    changed: &std::collections::HashSet<String>,
    dep_changed: bool,
    glbs: &std::collections::HashSet<String>,
    collapsed_names: &HashMap<String, String>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(name) = name.to_str() else { continue };
        let raw = name.strip_suffix(".br").unwrap_or(name);

        let h = if raw.starts_with("xn-") {
            match collapsed_names.get(raw) {
                Some(h) => h.clone(),
                None => {
                    let _ = std::fs::remove_file(ent.path());
                    continue;
                }
            }
        } else {
            let (_, stem) = crate::resolver::split_platform(raw);
            let (hash, _) = naming::split_bundle_stem(stem);
            hash.to_lowercase()
        };
        if changed.contains(&h) || (dep_changed && glbs.contains(&h)) {
            let _ = std::fs::remove_file(ent.path());
        }
    }
}

fn build_id() -> String {
    let mut buf: Vec<u8> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Ok(b) = std::fs::read(&exe) {
            buf.extend_from_slice(&b);
        }
    }
    buf.extend_from_slice(crate::builder::template_identity().as_bytes());
    crate::hashes::sha256_hex(&buf)
}

fn iso_from_build_id(id: &str) -> String {
    let n = u64::from_str_radix(id.get(..8).unwrap_or("0"), 16).unwrap_or(0);
    let base = 1_577_836_800u64;
    crate::dates::iso8601_utc(base + (n % 946_080_000))
}

pub fn build_scoped_date() -> String {
    use std::sync::OnceLock;
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(|| iso_from_build_id(&build_id())).clone()
}

pub struct ProxyConfig {
    pub catalyst_url: String,

    pub local_root: Option<String>,

    pub cache_dir: String,
    pub version: String,
    pub date: Option<String>,
    pub parity: bool,
    pub magenta_missing: bool,
    pub fallback_version: String,
    pub use_space: bool,

    pub asset_reuse: bool,

    pub template_root: Option<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            catalyst_url: crate::catalyst::DEFAULT_CATALYST.to_string(),
            local_root: None,
            cache_dir: "./abgen-serve-cache".to_string(),
            version: "v41".to_string(),
            date: None,
            parity: false,
            magenta_missing: false,
            fallback_version: "v41".to_string(),
            use_space: false,
            asset_reuse: true,
            template_root: None,
        }
    }
}

impl Proxy {
    pub fn new(cfg: ProxyConfig) -> Arc<Self> {
        Self::new_with_space(cfg, None)
    }

    pub fn new_with_space(cfg: ProxyConfig, injected_space: Option<Arc<Space>>) -> Arc<Self> {
        let collection_mode = BuildOpts::env_collection_mode();
        let real_textures = !cfg.parity || BuildOpts::env_real_textures();
        let v38_compat = !cfg.parity || BuildOpts::env_v38_compat();
        let v38_timestamp = BuildOpts::env_v38_timestamp();
        let magenta_missing = cfg.magenta_missing || BuildOpts::env_magenta_missing();
        let asset_reuse = crate::clihelp::env_bool("ABGEN_DEPS_DIGEST", cfg.asset_reuse);
        if let Some(root) = cfg.template_root.as_deref().filter(|s| !s.is_empty()) {
            let env_root = std::env::var("ABGEN_ROOT").unwrap_or_default();
            if env_root.trim() != root {
                tracing::warn!(
                    template_root = %root,
                    abgen_root_env = %env_root,
                    "template_root differs from the ABGEN_ROOT env — builder templates \
                     resolve from ABGEN_ROOT, or from the copy compiled into this \
                     build when it is unset; set ABGEN_ROOT at process start"
                );
            }
        }
        let bid = build_id();
        let date = cfg.date.unwrap_or_else(|| iso_from_build_id(&bid));
        let cache_root = PathBuf::from(&cfg.cache_dir);
        let content = LocalContentStore::new(cache_root.join("content"));
        let digests_dir = cache_root.join("content-digests");
        let bundle_dir = cache_root.join("bundles").join(&bid[..16.min(bid.len())]);
        let _ = std::fs::create_dir_all(&bundle_dir);
        let space = match injected_space {
            Some(s) => Some(s),
            None if cfg.use_space => {
                let s = Space::from_env().map(Arc::new);
                if s.is_none() {
                    tracing::warn!(
                        "S3 space cache requested (use_space) but disabled: endpoint/credentials \
                         missing (set ABGEN_S3_ENDPOINT and credentials)"
                    );
                }
                s
            }
            None => None,
        };
        Arc::new(Proxy {
            catalyst: {
                let mut c = CatalystClient::from_args(&cfg.catalyst_url, cfg.local_root.as_deref());
                if let Some(wurl) = crate::worlds::content_fallback_from_env() {
                    tracing::info!(url = %wurl, "worlds content fallback ENABLED");
                    c = c.with_fallback_base(wurl);
                }
                c
            },
            local: cfg.local_root.map(LocalContentStore::new),
            content,
            bundle_dir,
            digests_dir,
            version: cfg.version,
            date,
            uri_cache: UriCache::new(),
            space,
            fallback_version: cfg.fallback_version,
            entities: Mutex::new(HashMap::new()),
            hash_index: Mutex::new(HashMap::new()),
            entity_cap: std::env::var("ABGEN_ENTITY_CACHE_CAP")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(4096),
            hash_index_cap: std::env::var("ABGEN_HASH_INDEX_CAP")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(65536),
            entity_locks: KeyedLocks::default(),
            bundle_locks: KeyedLocks::default(),
            collection_mode,
            real_textures,
            v38_compat,
            v38_timestamp,
            magenta_missing,
            jit_cache: OnceLock::new(),
            asset_reuse,
            build_progress: Mutex::new(HashMap::new()),
            decode_ok: Mutex::new(HashMap::new()),
        })
    }

    pub fn turbojpeg_available() -> bool {
        crate::ffi::turbojpeg_available()
    }
}

#[cfg(test)]
pub mod stub {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    pub type Routes = Vec<(String, u16, Vec<u8>)>;

    pub fn stub_proxy_at(
        host: &str,
        catalyst_url: &str,
        read_only: bool,
        cache_dir: &std::path::Path,
    ) -> Arc<super::Proxy> {
        stub_proxy_at_reuse(host, catalyst_url, read_only, cache_dir, false)
    }

    pub fn stub_proxy_at_reuse(
        host: &str,
        catalyst_url: &str,
        read_only: bool,
        cache_dir: &std::path::Path,
        asset_reuse: bool,
    ) -> Arc<super::Proxy> {
        let space = crate::space::Space::with_static_creds(
            "http",
            host,
            "us-east-1",
            None,
            false,
            read_only,
            "AKIATEST",
            "secret",
        );
        let cfg = super::ProxyConfig {
            catalyst_url: catalyst_url.to_string(),
            cache_dir: cache_dir.to_string_lossy().into_owned(),
            version: "v41".to_string(),
            fallback_version: "v40".to_string(),
            asset_reuse,
            ..Default::default()
        };
        super::Proxy::new_with_space(cfg, Some(Arc::new(space)))
    }

    pub fn serve(routes: Routes) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { break };
                let mut reader = BufReader::new(match stream.try_clone() {
                    Ok(r) => r,
                    Err(_) => continue,
                });
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let mut content_len = 0usize;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() {
                        break;
                    }
                    let ht = h.trim();
                    if ht.is_empty() {
                        break;
                    }
                    if let Some(v) = ht.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                }
                if content_len > 0 {
                    let mut body = vec![0u8; content_len];
                    let _ = reader.read_exact(&mut body);
                }
                seen2.lock().unwrap().push(format!("{method} {path}"));
                let (code, body) = routes
                    .iter()
                    .find(|(p, _, _)| path == *p)
                    .map(|(_, c, b)| (*c, b.clone()))
                    .unwrap_or((404, Vec::new()));
                let reason = match code {
                    200 => "OK",
                    404 => "Not Found",
                    _ => "Error",
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        (format!("127.0.0.1:{}", addr.port()), seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cache(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("abgen-live-test-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn stub_proxy(host: &str, read_only: bool, tag: &str) -> Arc<Proxy> {
        super::stub::stub_proxy_at(host, "http://127.0.0.1:9", read_only, &temp_cache(tag))
    }

    #[test]
    fn metadata_only_files_leave_the_conversion_input() {
        let scene = Scene {
            entity_id: "bafkentity".into(),
            entity_type: "scene".into(),
            pointers: vec![],
            content: vec![
                crate::catalyst::ContentEntry {
                    file: "models/a.glb".into(),
                    hash: "h_glb".into(),
                },
                crate::catalyst::ContentEntry {
                    file: "tex/real.png".into(),
                    hash: "h_tex".into(),
                },
                crate::catalyst::ContentEntry {
                    file: "assets/autogenerated-thumbnail.png".into(),
                    hash: "h_auto".into(),
                },
                crate::catalyst::ContentEntry {
                    file: "assets/navmap.png".into(),
                    hash: "h_navmap".into(),
                },
                crate::catalyst::ContentEntry {
                    file: "bin/game.js".into(),
                    hash: "h_js".into(),
                },
            ],
            metadata: serde_json::json!({"display": {"navmapThumbnail": "assets/navmap.png"}}),
        };
        let skipped = scene.metadata_only_hashes();
        let names: Vec<&str> = convertible_entries(&scene, &skipped)
            .iter()
            .map(|c| c.file.as_str())
            .collect();
        assert_eq!(names, vec!["models/a.glb", "tex/real.png"]);

        let unfiltered = convertible_entries(&scene, &std::collections::HashSet::new());
        assert_eq!(unfiltered.len(), 4);
    }

    #[test]
    fn deps_digests_parallel_matches_serial() {
        let dir = temp_cache("deps-digest-par");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = LocalContentStore::new(&dir);

        let gltf = |uris: &[&str]| {
            let imgs: Vec<String> = uris
                .iter()
                .map(|u| format!("{{\"uri\":\"{u}\"}}"))
                .collect();
            format!(
                "{{\"asset\":{{\"version\":\"2.0\"}},\"images\":[{}]}}",
                imgs.join(",")
            )
        };
        store
            .write("g1", gltf(&["tex1.png", "mesh.bin"]).as_bytes())
            .unwrap();
        store.write("g2", gltf(&["tex2.png"]).as_bytes()).unwrap();
        store.write("gbroken", b"{not json").unwrap();
        store.write("gdup", gltf(&["tex1.png"]).as_bytes()).unwrap();

        let mut map: HashMap<String, String> = HashMap::new();
        map.insert("a/tex1.png".to_string(), "t1".to_string());
        map.insert("a/mesh.bin".to_string(), "m1".to_string());
        map.insert("a/tex2.png".to_string(), "t2".to_string());

        let items = vec![
            ("g1".to_string(), "a/one.gltf".to_string()),
            ("gdup".to_string(), "a/dup.gltf".to_string()),
            ("gdup".to_string(), "b/dup.gltf".to_string()),
            ("gbroken".to_string(), "a/broken.gltf".to_string()),
            ("t1".to_string(), "a/tex1.png".to_string()),
            ("g2".to_string(), "a/two.gltf".to_string()),
        ];

        let (d1, u1, w1) = compute_deps_digests(&store, &items, &map, false, 1);
        let (d8, u8_, w8) = compute_deps_digests(&store, &items, &map, false, 8);
        assert_eq!(d1, d8);
        assert_eq!(u1, u8_);
        assert_eq!(w1, w8);
        assert_eq!(d1.len(), 3);
        assert!(d1.contains_key("g1"));
        assert!(d1.contains_key("gdup"));
        assert!(d1.contains_key("g2"));
        assert_eq!(w1.len(), 1);
        assert_eq!(w1[0].0, "a/broken.gltf");
        assert_eq!(w1[0].1, "gbroken");
        assert!(u1.is_empty());
    }

    #[test]
    fn undeployed_dep_glbs_classified_skipped_not_failed() {
        let dir = temp_cache("deps-digest-undeployed");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = LocalContentStore::new(&dir);

        store
            .write(
                "ghat",
                b"{\"asset\":{\"version\":\"2.0\"},\"images\":[{\"uri\":\"missing-hat.png\"}]}",
            )
            .unwrap();
        store
            .write(
                "gok",
                b"{\"asset\":{\"version\":\"2.0\"},\"images\":[{\"uri\":\"tex1.png\"}]}",
            )
            .unwrap();
        store.write("gbroken", b"{not json").unwrap();

        let mut map: HashMap<String, String> = HashMap::new();
        map.insert("a/hat.gltf".to_string(), "ghat".to_string());
        map.insert("a/ok.gltf".to_string(), "gok".to_string());
        map.insert("a/broken.gltf".to_string(), "gbroken".to_string());
        map.insert("a/tex1.png".to_string(), "t1".to_string());

        let items = vec![
            ("ghat".to_string(), "a/hat.gltf".to_string()),
            ("gok".to_string(), "a/ok.gltf".to_string()),
            ("gbroken".to_string(), "a/broken.gltf".to_string()),
        ];

        let (digests, undeployed, warns) = compute_deps_digests(&store, &items, &map, false, 2);
        assert_eq!(digests.len(), 1);
        assert!(digests.contains_key("gok"));
        assert_eq!(
            undeployed.into_iter().collect::<Vec<_>>(),
            vec!["ghat".to_string()]
        );
        assert_eq!(warns.len(), 2);
    }

    #[test]
    fn image_decode_verdict_is_memoized_per_hash() {
        let cache_dir = temp_cache("decode-ok-memo");
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();
        let local_dir = cache_dir.join("src-local");
        let local = LocalContentStore::new(&local_dir);

        let mut png = Vec::new();
        image::RgbaImage::new(1, 1)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        local.write("goodimg", &png).unwrap();
        local.write("badimg", b"not an image at all").unwrap();

        let proxy = Proxy::new(ProxyConfig {
            catalyst_url: "http://127.0.0.1:9".to_string(),
            local_root: Some(local_dir.to_string_lossy().into_owned()),
            cache_dir: cache_dir.to_string_lossy().into_owned(),
            ..Default::default()
        });

        assert!(proxy.image_decode_ok("goodimg"));
        assert!(!proxy.image_decode_ok("badimg"));
        assert!(crate::builder::source_image_decodes(&png));
        assert!(!crate::builder::source_image_decodes(
            b"not an image at all"
        ));

        let _ = std::fs::remove_dir_all(proxy.cache_roots().0);
        let _ = std::fs::remove_dir_all(&local_dir);
        assert!(proxy.image_decode_ok("goodimg"));
        assert!(!proxy.image_decode_ok("badimg"));

        assert!(proxy.image_decode_ok("lateimg"));
        let local = LocalContentStore::new(&local_dir);
        local.write("lateimg", b"still not an image").unwrap();
        assert!(!proxy.image_decode_ok("lateimg"));
    }

    #[test]
    fn space_get_bundle_continues_to_fallback_after_transport_error() {
        let (host, seen) = super::stub::serve(vec![
            ("/v41/bafkcid/Qmhash_windows".to_string(), 500, Vec::new()),
            (
                "/v40/bafkcid/Qmhash_windows".to_string(),
                200,
                b"FALLBACK".to_vec(),
            ),
        ]);
        let proxy = stub_proxy(&host, false, "bug5");
        let got = proxy.space_get_bundle("bafkcid", "Qmhash_windows");
        assert_eq!(got.as_deref(), Some(&b"FALLBACK"[..]));
        let log = seen.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "GET /v41/bafkcid/Qmhash_windows".to_string(),
                "GET /v40/bafkcid/Qmhash_windows".to_string(),
            ]
        );
    }

    #[test]
    fn space_key_helpers_roundtrip_and_respect_read_only() {
        let (host, seen) = super::stub::serve(vec![
            (
                "/LOD/1/bafk_1_windows".to_string(),
                200,
                b"LODBYTES".to_vec(),
            ),
            ("/v41/flatalias_windows".to_string(), 200, Vec::new()),
        ]);
        let proxy = stub_proxy(&host, false, "keys");
        assert_eq!(
            proxy.space_get_key("LOD/1/bafk_1_windows").as_deref(),
            Some(&b"LODBYTES"[..])
        );
        assert_eq!(proxy.space_get_key("LOD/1/other_1_windows"), None);
        proxy.space_put_key("v41/flatalias_windows", b"X");
        let log = seen.lock().unwrap().clone();
        assert!(
            log.contains(&"PUT /v41/flatalias_windows".to_string()),
            "{log:?}"
        );

        let (host_ro, seen_ro) = super::stub::serve(vec![]);
        let ro = stub_proxy(&host_ro, true, "keys-ro");
        ro.space_put_key("v41/never_windows", b"X");
        assert!(seen_ro.lock().unwrap().is_empty());
    }

    fn stub_proxy_reuse(host: &str, tag: &str) -> Arc<Proxy> {
        super::stub::stub_proxy_at_reuse(host, "http://127.0.0.1:9", false, &temp_cache(tag), true)
    }

    #[test]
    fn asset_reuse_puts_and_gets_shared_assets_layout() {
        let (host, seen) = super::stub::serve(vec![(
            "/v40/assets/Qmhash_0123456789abcdef0123456789abcdef_windows".to_string(),
            200,
            b"SHARED".to_vec(),
        )]);
        let proxy = stub_proxy_reuse(&host, "reuse-layout");
        proxy.space_put_bundle(
            "bafkcid",
            "Qmhash_0123456789abcdef0123456789abcdef_windows",
            b"B",
        );
        let got =
            proxy.space_get_bundle("bafkcid", "Qmhash_0123456789abcdef0123456789abcdef_windows");
        assert_eq!(got.as_deref(), Some(&b"SHARED"[..]));
        let log = seen.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "PUT /v41/assets/Qmhash_0123456789abcdef0123456789abcdef_windows".to_string(),
                "GET /v41/assets/Qmhash_0123456789abcdef0123456789abcdef_windows".to_string(),
                "GET /v41/bafkcid/Qmhash_0123456789abcdef0123456789abcdef_windows".to_string(),
                "GET /v40/assets/Qmhash_0123456789abcdef0123456789abcdef_windows".to_string(),
            ]
        );
    }

    #[test]
    fn asset_reuse_probe_heads_shared_key() {
        let (host, seen) = super::stub::serve(vec![(
            "/v41/assets/Qmhit_0123456789abcdef0123456789abcdef_windows".to_string(),
            200,
            Vec::new(),
        )]);
        let proxy = stub_proxy_reuse(&host, "reuse-probe");
        assert!(proxy.space_probe_asset("Qmhit_0123456789abcdef0123456789abcdef_windows"));
        assert!(!proxy.space_probe_asset("Qmmiss_0123456789abcdef0123456789abcdef_windows"));
        let log = seen.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "HEAD /v41/assets/Qmhit_0123456789abcdef0123456789abcdef_windows".to_string(),
                "HEAD /v41/assets/Qmmiss_0123456789abcdef0123456789abcdef_windows".to_string(),
            ]
        );

        let (host2, seen2) = super::stub::serve(vec![]);
        let legacy = stub_proxy(&host2, false, "legacy-probe");
        assert!(!legacy.space_probe_asset("Qmhit_0123456789abcdef0123456789abcdef_windows"));
        assert!(seen2.lock().unwrap().is_empty());
    }

    #[test]
    fn legacy_mode_keeps_entity_scoped_puts() {
        let (host, seen) = super::stub::serve(vec![]);
        let proxy = stub_proxy(&host, false, "legacy-put");
        proxy.space_put_bundle("bafkcid", "Qmhash_windows", b"B");
        let log = seen.lock().unwrap().clone();
        assert_eq!(log, vec!["PUT /v41/bafkcid/Qmhash_windows".to_string()]);
    }

    #[test]
    fn bounded_reserve_evicts_only_past_cap() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("a".to_string(), 1);
        m.insert("b".to_string(), 2);
        bounded_reserve(&mut m, 2, "c");
        assert_eq!(m.len(), 1);
        m.insert("c".to_string(), 3);
        assert!(m.contains_key("c"));
        bounded_reserve(&mut m, 2, "c");
        assert_eq!(m.len(), 2);
        assert!(m.contains_key("c"));
    }

    #[test]
    fn content_build_cache_is_lru_bounded_and_self_heals() {
        use crate::jitcache::JitDiskCache;

        let cache_dir = temp_cache("jit-content-bound");
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();
        let local_dir = cache_dir.join("src-local");
        let local = LocalContentStore::new(&local_dir);
        for h in ["hasha", "hashb", "hashc"] {
            local.write(h, &[0u8; 100]).unwrap();
        }

        let cfg = ProxyConfig {
            catalyst_url: "http://127.0.0.1:9".to_string(),
            local_root: Some(local_dir.to_string_lossy().into_owned()),
            cache_dir: cache_dir.to_string_lossy().into_owned(),
            version: "v41".to_string(),
            fallback_version: "v40".to_string(),
            ..Default::default()
        };
        let proxy = Proxy::new(cfg);
        let cache = JitDiskCache::new(250);
        proxy.set_jit_cache(cache.clone());

        let content = LocalContentStore::new(proxy.cache_roots().0);

        proxy.ensure_content("hasha").unwrap();
        proxy.ensure_content("hashb").unwrap();
        proxy.ensure_content("hasha").unwrap();
        proxy.ensure_content("hashc").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(80));

        assert!(
            cache.total_bytes() <= 250,
            "content cache must stay within budget: {}",
            cache.total_bytes()
        );
        assert!(content.exists("hasha"), "hasha was touched, must survive");
        assert!(content.exists("hashc"), "hashc is newest, must survive");
        assert!(
            !content.exists("hashb"),
            "hashb is LRU, must be evicted off disk"
        );

        proxy.ensure_content("hashb").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(
            content.exists("hashb"),
            "evicted hashb must self-heal (refetch) on next ensure_content"
        );
        assert!(
            cache.total_bytes() <= 250,
            "still bounded after self-heal: {}",
            cache.total_bytes()
        );

        let _ = std::fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn refresh_entity_content_prunes_stale_conversions() {
        let cache = temp_cache("revalidate");
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(&cache).unwrap();
        let corpus = cache.join("corpus");

        let entity = serde_json::json!({
            "id": "bafkreva",
            "type": "scene",
            "content": [
                {"file": "m.glb", "hash": "qmglb"},
                {"file": "tex.png", "hash": "qmtex"},
            ]
        });
        let ent_bytes = serde_json::to_vec(&entity).unwrap();

        let mk = |glb: &[u8], tex: &[u8]| {
            let (host, _seen) = super::stub::serve(vec![
                ("/contents/bafkreva".to_string(), 200, ent_bytes.clone()),
                ("/contents/qmglb".to_string(), 200, glb.to_vec()),
                ("/contents/qmtex".to_string(), 200, tex.to_vec()),
            ]);
            Proxy::new(ProxyConfig {
                catalyst_url: format!("http://{host}"),
                cache_dir: cache.to_string_lossy().into_owned(),
                ..Default::default()
            })
        };

        let p1 = mk(b"GLB1", b"TEX1");
        let c1 = p1.refresh_entity_content("bafkreva", &corpus).unwrap();
        assert_eq!(c1.len(), 2);

        let bundle_dir = p1.bundle_dir.join("bafkreva");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        let glb_bundle = bundle_dir.join("qmglb_0123456789abcdef0123456789abcdef_windows");
        let tex_bundle = bundle_dir.join("qmtex_windows");
        std::fs::write(&glb_bundle, b"AB").unwrap();
        std::fs::write(&tex_bundle, b"AB").unwrap();
        let corpus_entity = corpus.join("bafkreva");
        std::fs::create_dir_all(&corpus_entity).unwrap();
        std::fs::write(corpus_entity.join("windows.manifest.json"), b"{}").unwrap();

        let p2 = mk(b"GLB1", b"TEX1");
        assert!(p2
            .refresh_entity_content("bafkreva", &corpus)
            .unwrap()
            .is_empty());
        assert!(glb_bundle.is_file());
        assert!(tex_bundle.is_file());
        assert!(corpus_entity.is_dir());

        let p3 = mk(b"GLB2", b"TEX1");
        let c3 = p3.refresh_entity_content("bafkreva", &corpus).unwrap();
        assert_eq!(c3, vec!["qmglb".to_string()]);
        assert!(!glb_bundle.exists());
        assert!(tex_bundle.is_file());
        assert!(!corpus_entity.exists());
        assert_eq!(p3.content.fetch("qmglb").unwrap(), b"GLB2");

        std::fs::write(&glb_bundle, b"AB").unwrap();
        std::fs::create_dir_all(&corpus_entity).unwrap();
        let p4 = mk(b"GLB2", b"TEX2");
        let c4 = p4.refresh_entity_content("bafkreva", &corpus).unwrap();
        assert_eq!(c4, vec!["qmtex".to_string()]);
        assert!(!glb_bundle.exists());
        assert!(!tex_bundle.exists());

        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn refresh_entity_content_trusts_content_versioned_ids() {
        let cache = temp_cache("revalidate_versioned");
        let _ = std::fs::remove_dir_all(&cache);
        std::fs::create_dir_all(&cache).unwrap();
        let corpus = cache.join("corpus");

        let v1 = "b64-L3MvbS5nbGIAMTIzLWhvc3Q=";
        let v2 = "b64-L3MvbS5nbGIANDU2LWhvc3Q=";

        let mk = |vid: &str, tex: &[u8]| {
            let entity = serde_json::json!({
                "id": "bafkreva",
                "type": "scene",
                "content": [
                    {"file": "m.glb", "hash": vid},
                    {"file": "tex.png", "hash": "qmtex"},
                ]
            });
            let (host, seen) = super::stub::serve(vec![
                (
                    "/contents/bafkreva".to_string(),
                    200,
                    serde_json::to_vec(&entity).unwrap(),
                ),
                ("/contents/qmtex".to_string(), 200, tex.to_vec()),
            ]);
            let p = Proxy::new(ProxyConfig {
                catalyst_url: format!("http://{host}"),
                cache_dir: cache.to_string_lossy().into_owned(),
                ..Default::default()
            });
            (p, seen)
        };

        let (p1, seen1) = mk(v1, b"TEX1");
        let c1 = p1.refresh_entity_content("bafkreva", &corpus).unwrap();
        assert_eq!(c1, vec![v1.to_string(), "qmtex".to_string()]);
        assert!(
            !seen1.lock().unwrap().iter().any(|p| p.contains("b64-")),
            "versioned id must never be fetched: {:?}",
            seen1.lock().unwrap()
        );

        let (p2, seen2) = mk(v1, b"TEX1");
        assert!(p2
            .refresh_entity_content("bafkreva", &corpus)
            .unwrap()
            .is_empty());
        assert!(!seen2.lock().unwrap().iter().any(|p| p.contains("b64-")));

        let (p3, seen3) = mk(v2, b"TEX1");
        let c3 = p3.refresh_entity_content("bafkreva", &corpus).unwrap();
        assert_eq!(c3, vec![v2.to_string()]);
        assert!(!seen3.lock().unwrap().iter().any(|p| p.contains("b64-")));

        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn probe_versions_dedup_and_hash_index() {
        let (host, _seen) = super::stub::serve(vec![]);
        let proxy = stub_proxy(&host, false, "probe");
        assert_eq!(
            proxy.space_probe_versions("v39"),
            vec!["v39".to_string(), "v41".to_string(), "v40".to_string()]
        );
        assert_eq!(
            proxy.space_probe_versions("v41"),
            vec!["v41".to_string(), "v40".to_string()]
        );
        assert_eq!(proxy.entity_for_hash("QmAbC"), None);
        proxy.index_content_hashes(vec![("QmAbC".to_string(), "bafkowner".to_string())]);
        assert_eq!(proxy.entity_for_hash("qmabc").as_deref(), Some("bafkowner"));
        assert_eq!(proxy.entity_for_hash("QmAbC").as_deref(), Some("bafkowner"));
    }
}

#[cfg(test)]
mod prune_tests {
    use super::*;

    #[test]
    fn prune_uses_names_index_for_collapsed_entries() {
        let dir = std::env::temp_dir().join(format!("abgen-prune-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in [
            "xn-changed",
            "xn-changed.br",
            "xn-kept",
            "xn-kept.br",
            "xn-unknown",
            "HASHA_mac",
            "HASHB_mac",
        ] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        let changed: std::collections::HashSet<String> = ["hasha".to_string(), "hxn1".to_string()]
            .into_iter()
            .collect();
        let glbs: std::collections::HashSet<String> = Default::default();
        let mut index: HashMap<String, String> = HashMap::new();
        index.insert("xn-changed".to_string(), "hxn1".to_string());
        index.insert("xn-kept".to_string(), "hxn2".to_string());

        prune_stale_bundles(&dir, &changed, false, &glbs, &index);

        assert!(!dir.join("xn-changed").exists());
        assert!(!dir.join("xn-changed.br").exists());
        assert!(dir.join("xn-kept").exists());
        assert!(dir.join("xn-kept.br").exists());
        assert!(!dir.join("xn-unknown").exists());
        assert!(!dir.join("HASHA_mac").exists());
        assert!(dir.join("HASHB_mac").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
