use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use super::lodjit::LodJit;
use crate::catalyst::CatalystClient;
use crate::jitcache::JitDiskCache;
use dcl_contents::content::ContentComponent;
use moka::future::Cache;

pub type ResolveCache = Cache<String, Option<(PathBuf, u64)>>;

pub struct IndexBuild {
    pub eager: bool,
    pub platforms: Vec<String>,
    pub sem: Arc<tokio::sync::Semaphore>,
    pub deadline: Duration,
    pub max_queue: usize,
    pub pending: Arc<AtomicUsize>,
}

impl IndexBuild {
    pub fn disabled() -> Self {
        Self {
            eager: false,
            platforms: Vec::new(),
            sem: Arc::new(tokio::sync::Semaphore::new(1)),
            deadline: Duration::from_secs(0),
            max_queue: 0,
            pending: Arc::new(AtomicUsize::new(0)),
        }
    }
}

pub struct AppStateInner {
    pub out_root: PathBuf,

    pub jit_root: PathBuf,

    pub jit_cache: Arc<JitDiskCache>,

    pub roots_distinct: bool,

    pub resolve_cache: ResolveCache,

    pub content: CatalystClient,

    pub bundle_index: HashMap<String, PathBuf>,

    pub live_proxy: Option<Arc<crate::live::Proxy>>,

    pub manifest_content_server_url: String,

    pub live_template_ok: bool,

    pub templates_missing: Vec<String>,

    pub ab_version: String,

    pub ab_date: String,
    pub content_db: Option<ContentComponent>,

    pub contents_registry: Option<dcl_contents::registry::RegistryAppState>,

    pub catalyst_url: String,

    pub out_root_writable: bool,

    pub lod_jit: LodJit,

    pub index_build: IndexBuild,

    pub worlds_content_url: Option<String>,

    pub shader_jit: bool,

    pub hash_neg_cache: Cache<String, ()>,

    pub jit_fail_cache: Cache<String, ()>,

    pub jit_inflight: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,

    pub jit_content_digest: bool,

    pub upstream_ab_cdn: Option<String>,

    pub upstream_ab_registry: Option<String>,

    pub upstream_registry_cache: Cache<String, Option<UpstreamAbRecord>>,

    pub revalidate_recent: Cache<String, ()>,

    pub revalidate_inflight: std::sync::Mutex<std::collections::HashSet<String>>,
}

#[derive(Clone, Debug)]
pub struct UpstreamAbRecord {
    pub versions: serde_json::Value,
    pub bundles: serde_json::Value,
    pub status: serde_json::Value,
}

pub type AppState = Arc<AppStateInner>;

fn env_ttl(name: &str, default_secs: u64) -> Duration {
    let secs = std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(default_secs);
    Duration::from_secs(secs.max(1))
}

impl AppStateInner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        out_root: PathBuf,
        content: CatalystClient,
        bundle_index: HashMap<String, PathBuf>,
        live_proxy: Option<Arc<crate::live::Proxy>>,
        manifest_content_server_url: String,
        live_template_ok: bool,
        templates_missing: Vec<String>,
        ab_version: String,
        ab_date: String,
        catalyst_url: String,
        out_root_writable: bool,
        lod_jit: LodJit,
        index_build: IndexBuild,
    ) -> Self {
        Self {
            jit_root: out_root.clone(),
            jit_cache: JitDiskCache::new(0),
            roots_distinct: false,
            out_root,
            resolve_cache: Cache::builder()
                .max_capacity(50_000)
                .time_to_live(Duration::from_secs(60))
                .build(),
            content,
            bundle_index,
            live_proxy,
            manifest_content_server_url,
            live_template_ok,
            templates_missing,
            ab_version,
            ab_date,
            content_db: None,
            contents_registry: None,
            catalyst_url,
            out_root_writable,
            lod_jit,
            index_build,
            worlds_content_url: None,
            shader_jit: crate::clihelp::env_bool("ABGEN_SHADER_JIT", true),
            hash_neg_cache: Cache::builder()
                .max_capacity(100_000)
                .time_to_live(env_ttl("ABGEN_HASH_RESOLVE_FAIL_TTL_S", 3600))
                .build(),
            jit_fail_cache: Cache::builder()
                .max_capacity(100_000)
                .time_to_live(env_ttl("ABGEN_JIT_FAIL_TTL_S", 60))
                .build(),
            jit_inflight: tokio::sync::Mutex::new(HashMap::new()),
            jit_content_digest: false,
            upstream_ab_cdn: None,
            upstream_ab_registry: None,
            upstream_registry_cache: Cache::builder()
                .max_capacity(100_000)
                .time_to_live(env_ttl("ABGEN_UPSTREAM_REGISTRY_TTL_S", 300))
                .build(),
            revalidate_recent: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(env_ttl("ABGEN_REVALIDATE_DEBOUNCE_S", 2))
                .build(),
            revalidate_inflight: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    pub fn with_dev_lanes(
        mut self,
        jit_content_digest: bool,
        upstream_ab_cdn: Option<String>,
    ) -> Self {
        self.jit_content_digest = jit_content_digest;
        self.upstream_ab_cdn = upstream_ab_cdn;
        self
    }

    pub fn with_upstream_ab_registry(mut self, url: Option<String>) -> Self {
        self.upstream_ab_registry = url;
        self
    }

    pub fn with_worlds_content_url(mut self, url: Option<String>) -> Self {
        self.worlds_content_url = url;
        self
    }

    pub fn with_jit(
        mut self,
        jit_root: PathBuf,
        jit_cache: Arc<JitDiskCache>,
        roots_distinct: bool,
    ) -> Self {
        self.jit_root = jit_root;
        self.jit_cache = jit_cache;
        self.roots_distinct = roots_distinct;
        self
    }

    pub fn serve_lookup(
        &self,
        build: impl Fn(&Path) -> Option<PathBuf>,
    ) -> Option<(PathBuf, bool)> {
        let out = build(&self.out_root);
        if let Some(p) = &out {
            if p.is_file() {
                return Some((p.clone(), false));
            }
        }
        if self.roots_distinct {
            if let Some(p) = build(&self.jit_root) {
                if p.is_file() {
                    return Some((p, true));
                }
            }
        }
        out.map(|p| (p, false))
    }

    /// The LRU key of a file under the JIT root: its first path component (an entity's
    /// directory), except under `bevy/`, whose objects are shared between entities and are
    /// accounted one file at a time, keyed by their path (`bevyvariants::cache_key`).
    pub fn jit_key_of(&self, exact: &Path) -> Option<String> {
        if let Some(key) = crate::bevyvariants::cache_key(&self.jit_root, exact) {
            return Some(key);
        }
        exact
            .strip_prefix(&self.jit_root)
            .ok()?
            .components()
            .next()
            .and_then(|c| c.as_os_str().to_str())
            .map(str::to_string)
    }

    pub fn touch_if_jit(&self, exact: &Path, from_jit: bool) {
        if from_jit {
            if let Some(key) = self.jit_key_of(exact) {
                self.jit_cache.touch(&key);
            }
        }
    }

    pub fn jit_record(&self, entity: &str) {
        if !self.jit_cache.enabled() {
            return;
        }
        let stored = crate::naming::fs_safe_component(entity);
        let dir = self.jit_root.join(&*stored);
        let bytes = crate::jitcache::dir_size(&dir);
        if bytes == 0 {
            return;
        }
        self.jit_cache.record(&stored, dir, bytes);
    }

    /// Account one Bevy-lane file under the JIT root by its own key (`jit_key_of`).
    pub fn jit_record_bevy_file(&self, path: &Path) {
        if let Some(key) = self.jit_key_of(path) {
            self.jit_record_file(&key, path);
        }
    }

    /// The JIT-root manifest of `entity` and the files it depends on: each variant it lists
    /// with its `.br` sidecar and `.json` record (whether or not they exist). Reads the
    /// manifest from disk, so call it off the async threads.
    pub fn bevy_manifest_deps(&self, entity: &str) -> (Option<PathBuf>, Vec<PathBuf>) {
        let manifest = crate::bevyvariants::manifest_path(&self.jit_root, entity);
        let deps = manifest
            .as_deref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<crate::bevyvariants::Manifest>(&b).ok())
            .map(|m| {
                m.files
                    .values()
                    .flatten()
                    .filter_map(|key| crate::bevyvariants::key_path(&self.jit_root, key))
                    .flat_map(|p| {
                        ["", ".br", ".json"].map(|suffix| {
                            let mut os = p.clone().into_os_string();
                            os.push(suffix);
                            PathBuf::from(os)
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        (manifest, deps)
    }

    /// Pin the LRU entries of `paths` (files under the JIT root) until the guards drop.
    pub fn pin_jit_paths(&self, paths: &[PathBuf]) -> Vec<crate::jitcache::PinGuard> {
        paths
            .iter()
            .filter_map(|p| self.jit_key_of(p))
            .filter_map(|k| self.jit_cache.pin(&k))
            .collect()
    }

    /// Pins on the JIT-root manifest of `entity` as it is on disk now and on every file it
    /// depends on: nothing it names can be evicted until the guards drop. Taken before a build
    /// probes for a current manifest, so a coalesced request serves it under these pins and a
    /// build starts with the previous manifest's files held. Empty when the cache is off.
    /// Reads the manifest from disk, so call it off the async threads.
    pub fn pin_bevy_manifest(&self, entity: &str) -> Vec<crate::jitcache::PinGuard> {
        if !self.jit_cache.enabled() {
            return Vec::new();
        }
        let (manifest, mut deps) = self.bevy_manifest_deps(entity);
        deps.extend(manifest);
        self.pin_jit_paths(&deps)
    }

    /// After a Bevy build (successful or not): account every file it wrote or reused under the
    /// JIT root (variants, sidecars, records, negative records, the manifest), each as its own
    /// LRU entry. Files outside the JIT root (a corpus `out_root`) are never accounted, so
    /// never evicted. Reads the manifest from disk, so call it off the async threads.
    ///
    /// Pins cover the whole hand-over. The build pinned each file as it reported it
    /// (`Proxy::take_bevy_written` carries those pins, held until this returns). Before
    /// recording anything, this pins the manifest and every file it lists (each variant with
    /// its `.br` and `.json`); it records the build's other files first, those files next and
    /// the manifest last, so they end up the most recently used entries and no record made
    /// here can evict them. The returned guards keep them pinned: a `?wait=1` request holds
    /// them until its manifest is served, everyone else drops them at once.
    ///
    /// Eviction still runs on every record, so the budget holds once the guards drop; a
    /// variant evicted later makes the manifest stale on a node without read-through
    /// (`bevy::manifest_is_current`), and the next request rebuilds that key.
    #[must_use = "the guards pin the manifest and its files until dropped"]
    pub fn jit_record_bevy(&self, entity: &str) -> Vec<crate::jitcache::PinGuard> {
        let Some(proxy) = self.live_proxy.as_ref() else {
            return Vec::new();
        };
        let written = proxy.take_bevy_written(entity);
        self.jit_record_bevy_written(entity, written)
    }

    /// [`Self::jit_record_bevy`] for an explicit set of written files.
    #[must_use = "the guards pin the manifest and its files until dropped"]
    pub fn jit_record_bevy_written(
        &self,
        entity: &str,
        written: crate::live::BevyWritten,
    ) -> Vec<crate::jitcache::PinGuard> {
        if !self.jit_cache.enabled() {
            return Vec::new();
        }
        let (manifest, deps) = self.bevy_manifest_deps(entity);
        let mut pinned = deps.clone();
        pinned.extend(manifest.clone());
        let pins = self.pin_jit_paths(&pinned);
        let dep_set: std::collections::HashSet<&PathBuf> = deps.iter().collect();
        for p in &written.paths {
            if !dep_set.contains(p) && Some(p) != manifest.as_ref() {
                self.jit_record_bevy_file(p);
            }
        }
        for p in &deps {
            self.jit_record_bevy_file(p);
        }
        if let Some(mp) = &manifest {
            self.jit_record_bevy_file(mp);
        }
        // The build's own pins (`written.pins`) drop here, after the manifest's are taken.
        pins
    }

    pub fn jit_record_file(&self, key: &str, path: &Path) {
        if !self.jit_cache.enabled() {
            return;
        }
        let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if bytes == 0 {
            return;
        }
        self.jit_cache.record(key, path.to_path_buf(), bytes);
    }
    pub fn with_content_db(mut self, content_db: Option<ContentComponent>) -> Self {
        self.content_db = content_db;
        self
    }

    pub fn with_registry_state(
        mut self,
        registry: Option<dcl_contents::registry::RegistryAppState>,
    ) -> Self {
        self.contents_registry = registry;
        self
    }
}
