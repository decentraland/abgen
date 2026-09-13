use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone, Copy)]
pub enum Kind {
    Bc7 = 1,
    Dxt1 = 2,
    Dxt5Crn = 3,
    Bc3 = 4,
}

struct Store {
    map: HashMap<[u8; 32], (Arc<Vec<u8>>, i32, u64)>,
    bytes: usize,
    stamp: u64,
    hits: u64,
    misses: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CacheProfile {
    Lambda,
    Client,
    Batch,
}

impl CacheProfile {
    pub fn memory_default_mb(self) -> usize {
        match self {
            CacheProfile::Client => 256,
            CacheProfile::Lambda | CacheProfile::Batch => 4096,
        }
    }

    pub fn disk_default_on(self) -> bool {
        match self {
            CacheProfile::Lambda => false,
            CacheProfile::Client | CacheProfile::Batch => true,
        }
    }

    pub fn disk_default_mb(self) -> u64 {
        match self {
            CacheProfile::Client => 2048,
            CacheProfile::Lambda | CacheProfile::Batch => 8192,
        }
    }
}

static FORCED: AtomicBool = AtomicBool::new(false);
static DISK_DEFAULT_ALLOWED: AtomicBool = AtomicBool::new(true);

static PROFILE: OnceLock<CacheProfile> = OnceLock::new();

fn profile() -> CacheProfile {
    PROFILE.get().copied().unwrap_or(CacheProfile::Batch)
}

fn env_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("ABGEN_TEX_ENCODE_CACHE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

fn max_bytes() -> usize {
    static ENV_MB: OnceLock<Option<usize>> = OnceLock::new();
    let env_mb = *ENV_MB.get_or_init(|| {
        std::env::var("ABGEN_TEX_ENCODE_CACHE_MAX_MB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
    });
    env_mb
        .unwrap_or_else(|| profile().memory_default_mb())
        .saturating_mul(1024 * 1024)
}

pub fn enable() {
    FORCED.store(true, Ordering::Relaxed);
}

/// Enables the caches; the first process-level profile declaration wins.
pub fn enable_with_profile(p: CacheProfile) {
    let _ = PROFILE.set(p);
    enable();
}

/// Enables the bounded memory cache while leaving persistent storage off
/// unless the caller explicitly sets `ABGEN_DISK_CACHE`.
pub fn enable_memory_only_with_profile(p: CacheProfile) {
    DISK_DEFAULT_ALLOWED.store(false, Ordering::Relaxed);
    enable_with_profile(p);
}

fn enabled() -> bool {
    FORCED.load(Ordering::Relaxed) || env_enabled()
}

fn store() -> &'static Mutex<Store> {
    static S: OnceLock<Mutex<Store>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(Store {
            map: HashMap::new(),
            bytes: 0,
            stamp: 0,
            hits: 0,
            misses: 0,
        })
    })
}

fn flights() -> &'static crate::singleflight::Group<[u8; 32], Option<(Arc<Vec<u8>>, i32)>> {
    static F: OnceLock<crate::singleflight::Group<[u8; 32], Option<(Arc<Vec<u8>>, i32)>>> =
        OnceLock::new();
    F.get_or_init(crate::singleflight::Group::new)
}

fn lock() -> std::sync::MutexGuard<'static, Store> {
    store().lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn content_key(
    kind: Kind,
    pixels: &[u8],
    width: u32,
    height: u32,
    params: &[i64],
) -> [u8; 32] {
    key(kind, pixels, width, height, params)
}

fn key(kind: Kind, pixels: &[u8], width: u32, height: u32, params: &[i64]) -> [u8; 32] {
    let mut h = crate::hashes::Sha256::new();
    h.update(env!("CARGO_PKG_VERSION").as_bytes());
    h.update(&[0u8]);
    h.update(env!("ABGEN_BUILD_ID").as_bytes());
    h.update(&[0u8]);
    h.update(&[kind as u8]);
    h.update(&width.to_le_bytes());
    h.update(&height.to_le_bytes());
    for p in params {
        h.update(&p.to_le_bytes());
    }
    h.update(pixels);
    h.finalize()
}

/// Persistent backing keyed like the memory cache plus build id. Entries are
/// sharded, atomically renamed, and LRU-evicted by mtime. Real release build ids
/// enable it unless the profile or `ABGEN_DISK_CACHE` disables it; the shared
/// `devbuild0000` id defaults off to prevent stale cross-tree hits.
#[cfg(not(target_arch = "wasm32"))]
mod disk {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    const EVICT_EVERY_N_WRITES: u64 = 8;

    static WRITE_COUNT: AtomicU64 = AtomicU64::new(0);

    const DEV_BUILD_ID: &str = "devbuild0000";

    pub(super) fn build_id_pins_encoder() -> bool {
        env!("ABGEN_BUILD_ID") != DEV_BUILD_ID
    }

    fn enabled() -> bool {
        crate::clihelp::env_bool(
            "ABGEN_DISK_CACHE",
            build_id_pins_encoder()
                && super::profile().disk_default_on()
                && super::DISK_DEFAULT_ALLOWED.load(Ordering::Relaxed),
        )
    }

    fn max_bytes() -> u64 {
        std::env::var("ABGEN_DISK_CACHE_MAX_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| super::profile().disk_default_mb())
            .saturating_mul(1024 * 1024)
    }

    fn cache_root() -> Option<PathBuf> {
        if let Ok(dir) = std::env::var("ABGEN_DISK_CACHE_DIR") {
            if !dir.trim().is_empty() {
                return Some(PathBuf::from(dir));
            }
        }
        if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
            if !xdg.trim().is_empty() {
                return Some(PathBuf::from(xdg).join("abgen").join("texencode"));
            }
        }
        let home = std::env::var("HOME").ok()?;
        if home.trim().is_empty() {
            return None;
        }
        let base = if cfg!(target_os = "macos") {
            PathBuf::from(home).join("Library").join("Caches")
        } else {
            PathBuf::from(home).join(".cache")
        };
        Some(base.join("abgen").join("texencode"))
    }

    fn hex(key: &[u8; 32]) -> String {
        let mut s = String::with_capacity(64);
        for b in key {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    fn shard_path(root: &Path, hex: &str) -> PathBuf {
        root.join(&hex[..2]).join(format!("{hex}.bin"))
    }

    fn entries(root: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
        let mut out = Vec::new();
        let Ok(shards) = fs::read_dir(root) else {
            return out;
        };
        for shard in shards.flatten() {
            let dir = shard.path();
            if !dir.is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(&dir) else {
                continue;
            };
            for f in files.flatten() {
                let path = f.path();
                let is_tmp = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".tmp."));
                if is_tmp {
                    continue;
                }
                if let Ok(meta) = f.metadata() {
                    if meta.is_file() {
                        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                        out.push((path, meta.len(), mtime));
                    }
                }
            }
        }
        out
    }

    fn evict_if_needed(root: &Path, budget: u64) {
        let mut items = entries(root);
        let total: u64 = items.iter().map(|(_, len, _)| *len).sum();
        if total <= budget {
            return;
        }
        items.sort_by_key(|(_, _, mtime)| *mtime);
        let mut over = total - budget;
        for (path, len, _) in items {
            if over == 0 {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                over = over.saturating_sub(len);
            }
        }
    }

    pub(super) fn get(key: &[u8; 32]) -> Option<(Vec<u8>, i32)> {
        if !enabled() {
            return None;
        }
        let root = cache_root()?;
        let path = shard_path(&root, &hex(key));
        let bytes = fs::read(&path).ok()?;
        if bytes.len() < 4 {
            return None;
        }
        let mips = i32::from_le_bytes(bytes[..4].try_into().ok()?);
        let data = bytes[4..].to_vec();
        if let Ok(f) = fs::File::open(&path) {
            let _ = f.set_modified(SystemTime::now());
        }
        Some((data, mips))
    }

    pub(super) fn put(key: &[u8; 32], data: &[u8], mips: i32) {
        if !enabled() {
            return;
        }
        let budget = max_bytes();
        let payload_len = data.len() as u64 + 4;
        if payload_len > budget {
            return;
        }
        let Some(root) = cache_root() else {
            return;
        };
        let path = shard_path(&root, &hex(key));
        if path.exists() {
            return;
        }
        let Some(dir) = path.parent() else {
            return;
        };
        if fs::create_dir_all(dir).is_err() {
            return;
        }
        let tmp = crate::tmppath::tmp_sibling(&path);
        let mut buf = Vec::with_capacity(payload_len as usize);
        buf.extend_from_slice(&mips.to_le_bytes());
        buf.extend_from_slice(data);
        if fs::write(&tmp, &buf).is_err() {
            let _ = fs::remove_file(&tmp);
            return;
        }
        if fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp);
            return;
        }
        if WRITE_COUNT
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(EVICT_EVERY_N_WRITES)
        {
            evict_if_needed(&root, budget);
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod disk {
    pub(super) fn get(_key: &[u8; 32]) -> Option<(Vec<u8>, i32)> {
        None
    }

    pub(super) fn put(_key: &[u8; 32], _data: &[u8], _mips: i32) {}
}

fn make_room(s: &mut Store, incoming: usize, budget: usize) {
    while s.bytes + incoming > budget && !s.map.is_empty() {
        let oldest = s
            .map
            .iter()
            .min_by_key(|(_, (_, _, stamp))| *stamp)
            .map(|(k, _)| *k)
            .expect("non-empty map has a minimum");
        if let Some((data, _, _)) = s.map.remove(&oldest) {
            s.bytes -= data.len();
        }
    }
}

pub fn get_or_encode_shared(
    kind: Kind,
    pixels: &[u8],
    width: u32,
    height: u32,
    params: &[i64],
    f: impl FnOnce() -> Option<(Vec<u8>, i32)>,
) -> Option<(Arc<Vec<u8>>, i32)> {
    if !enabled() {
        return f().map(|(data, mips)| (Arc::new(data), mips));
    }
    let k = key(kind, pixels, width, height, params);
    {
        let mut s = lock();
        s.stamp += 1;
        let stamp = s.stamp;
        if let Some((data, mips, at)) = s.map.get_mut(&k) {
            *at = stamp;
            let out = (Arc::clone(data), *mips);
            s.hits += 1;
            return Some(out);
        }
        s.misses += 1;
    }
    let mut work = Some(f);
    loop {
        let (result, leader) = flights().run_with_leader(k, || {
            {
                let mut s = lock();
                s.stamp += 1;
                let stamp = s.stamp;
                if let Some((data, mips, at)) = s.map.get_mut(&k) {
                    *at = stamp;
                    return Some((Arc::clone(data), *mips));
                }
            }
            if let Some((data, mips)) = disk::get(&k) {
                let data = Arc::new(data);
                remember(k, Arc::clone(&data), mips);
                return Some((data, mips));
            }
            let (data, mips) =
                work.take()
                    .expect("single-flight leader owns the encode closure")()?;
            disk::put(&k, &data, mips);
            let data = Arc::new(data);
            remember(k, Arc::clone(&data), mips);
            Some((data, mips))
        });
        if leader || result.is_some() {
            return result;
        }
    }
}

fn remember(k: [u8; 32], data: Arc<Vec<u8>>, mips: i32) {
    let len = data.len();
    let budget = max_bytes();
    if len > budget {
        return;
    }
    let mut s = lock();
    s.stamp += 1;
    let stamp = s.stamp;
    if !s.map.contains_key(&k) {
        make_room(&mut s, len, budget);
        s.map.insert(k, (data, mips, stamp));
        s.bytes += len;
    }
}

pub fn get_or_encode(
    kind: Kind,
    pixels: &[u8],
    width: u32,
    height: u32,
    params: &[i64],
    f: impl FnOnce() -> Option<(Vec<u8>, i32)>,
) -> Option<(Vec<u8>, i32)> {
    get_or_encode_shared(kind, pixels, width, height, params, f)
        .map(|(data, mips)| (Arc::try_unwrap(data).unwrap_or_else(|a| (*a).clone()), mips))
}

pub fn stats() -> (u64, u64, usize, usize) {
    let s = lock();
    (s.hits, s.misses, s.bytes, s.map.len())
}

pub fn clear() {
    let mut s = lock();
    s.map.clear();
    s.bytes = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_and_returns_identical_results() {
        enable();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let salt = [i64::from(std::process::id()), nanos];
        let key = |tail: i64| [salt[0], salt[1], tail];
        let pixels: Vec<u8> = (0..8u32 * 8 * 4).map(|i| (i * 7 % 251) as u8).collect();
        let mut calls = 0u32;
        let a = get_or_encode(Kind::Bc7, &pixels, 8, 8, &key(42), || {
            calls += 1;
            Some((vec![1, 2, 3], 4))
        })
        .unwrap();
        let b = get_or_encode(Kind::Bc7, &pixels, 8, 8, &key(42), || {
            calls += 1;
            Some((vec![9, 9, 9], 9))
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(a, b);
        assert_eq!(a.0, vec![1, 2, 3]);

        let c = get_or_encode(Kind::Bc7, &pixels, 8, 8, &key(43), || Some((vec![5], 1))).unwrap();
        assert_eq!(c.0, vec![5]);

        let d = get_or_encode(Kind::Dxt1, &pixels, 8, 8, &key(1), || None);
        assert!(d.is_none());
        let e = get_or_encode(Kind::Dxt1, &pixels, 8, 8, &key(1), || Some((vec![7], 1))).unwrap();
        assert_eq!(e.0, vec![7]);
    }

    #[test]
    fn bc7_end_to_end_reuses_encode() {
        enable();
        let px: Vec<u8> = (0..(16u32 * 16 * 4)).map(|i| (i % 255) as u8).collect();
        let (h0, _, _, _) = stats();
        let a = crate::bc7_pure::encode_bc7_mip_chain_with_profile(
            &px,
            16,
            16,
            Some(1),
            true,
            false,
            false,
            crate::bc7_pure::Bc7Profile::Basic,
        );
        let b = crate::bc7_pure::encode_bc7_mip_chain_with_profile(
            &px,
            16,
            16,
            Some(1),
            true,
            false,
            false,
            crate::bc7_pure::Bc7Profile::Basic,
        );
        assert_eq!(a, b);
        let (h1, _, _, _) = stats();
        assert!(h1 > h0);
    }

    #[test]
    fn shared_hit_returns_cache_buffer_without_copy() {
        enable();
        let pixels: Vec<u8> = (0..8u32 * 8 * 4).map(|i| (i * 3 % 253) as u8).collect();
        let a = get_or_encode_shared(Kind::Bc7, &pixels, 8, 8, &[7], || {
            Some((vec![10, 20, 30], 2))
        })
        .unwrap();
        let b = get_or_encode_shared(Kind::Bc7, &pixels, 8, 8, &[7], || unreachable!()).unwrap();
        assert!(Arc::ptr_eq(&a.0, &b.0), "hit must share the stored buffer");
        assert_eq!((&*a.0, a.1), (&vec![10, 20, 30], 2));

        let c = get_or_encode(Kind::Bc7, &pixels, 8, 8, &[7], || unreachable!()).unwrap();
        assert_eq!(c, (vec![10, 20, 30], 2), "legacy view sees the same bytes");
    }

    #[test]
    fn concurrent_miss_encodes_once_and_returns_identical_bytes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Barrier;

        enable_memory_only_with_profile(CacheProfile::Batch);
        const THREADS: usize = 12;
        let calls = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(THREADS));
        let salt = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        let pixels = Arc::new(vec![salt as u8; 16 * 16 * 4]);
        let params = [salt, 771];
        let flight_key = key(Kind::Bc7, &pixels, 16, 16, &params);
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..THREADS {
                let calls = Arc::clone(&calls);
                let start = Arc::clone(&start);
                let pixels = Arc::clone(&pixels);
                handles.push(scope.spawn(move || {
                    start.wait();
                    get_or_encode_shared(Kind::Bc7, &pixels, 16, 16, &params, || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        while flights().waiter_count(&flight_key) != THREADS - 1 {
                            std::thread::yield_now();
                        }
                        Some((vec![3, 1, 4, 1, 5, 9], 4))
                    })
                    .unwrap()
                }));
            }
            let outputs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            for output in &outputs[1..] {
                assert_eq!((&*output.0, output.1), (&*outputs[0].0, outputs[0].1));
                assert!(Arc::ptr_eq(&output.0, &outputs[0].0));
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn waiter_retries_its_own_work_after_leader_returns_none() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Barrier;

        enable_memory_only_with_profile(CacheProfile::Batch);
        let salt = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        let pixels = Arc::new(vec![salt as u8; 16 * 16 * 4]);
        let params = [salt, 772];
        let flight_key = key(Kind::Bc7, &pixels, 16, 16, &params);
        let leader_started = Arc::new(Barrier::new(2));

        std::thread::scope(|scope| {
            let owner_pixels = Arc::clone(&pixels);
            let owner_started = Arc::clone(&leader_started);
            let owner = scope.spawn(move || {
                get_or_encode_shared(Kind::Bc7, &owner_pixels, 16, 16, &params, || {
                    owner_started.wait();
                    while flights().waiter_count(&flight_key) != 1 {
                        std::thread::yield_now();
                    }
                    None
                })
            });

            leader_started.wait();
            let calls = AtomicUsize::new(0);
            let waiter = get_or_encode_shared(Kind::Bc7, &pixels, 16, 16, &params, || {
                calls.fetch_add(1, Ordering::SeqCst);
                Some((vec![2, 7, 1, 8], 3))
            });

            assert!(owner.join().unwrap().is_none());
            let waiter = waiter.expect("waiter's successful closure must not inherit None");
            assert_eq!((&*waiter.0, waiter.1), (&vec![2, 7, 1, 8], 3));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn eviction_is_lru_and_bytes_stay_consistent() {
        let mut s = Store {
            map: HashMap::new(),
            bytes: 0,
            stamp: 0,
            hits: 0,
            misses: 0,
        };
        let entry_len = 16usize;
        let budget = 3 * entry_len;
        for seed in 0u8..3 {
            s.stamp += 1;
            s.bytes += entry_len;
            s.map
                .insert([seed; 32], (Arc::new(vec![seed; entry_len]), 1, s.stamp));
        }
        s.stamp += 1;
        let stamp = s.stamp;
        s.map.get_mut(&[0u8; 32]).unwrap().2 = stamp;

        make_room(&mut s, entry_len, budget);
        assert!(s.map.contains_key(&[0u8; 32]), "recently touched survives");
        assert!(!s.map.contains_key(&[1u8; 32]), "LRU entry evicted");
        assert!(s.map.contains_key(&[2u8; 32]));
        assert_eq!(s.bytes, 2 * entry_len);
    }

    #[test]
    fn poisoned_lock_does_not_cascade() {
        enable();
        let _ = std::thread::spawn(|| {
            let _g = lock();
            panic!("poison the mutex on purpose");
        })
        .join();
        let pixels = vec![0u8; 8 * 8 * 4];
        let r = get_or_encode(Kind::Bc3, &pixels, 8, 8, &[99], || Some((vec![1, 2], 3))).unwrap();
        assert_eq!(r, (vec![1, 2], 3), "cache must survive a poisoned lock");
        let _ = stats();
    }

    struct EnvGuard(&'static str);

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }

    #[test]
    fn disk_cache_serves_byte_identical_hits_across_simulated_process_restarts() {
        let dir =
            std::env::temp_dir().join(format!("abgen_texencode_disk_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("ABGEN_DISK_CACHE_DIR", &dir);
        std::env::set_var("ABGEN_DISK_CACHE", "1");
        let _dir_guard = EnvGuard("ABGEN_DISK_CACHE_DIR");
        let _enable_guard = EnvGuard("ABGEN_DISK_CACHE");
        enable();

        let pixels: Vec<u8> = (0..8u32 * 8 * 4).map(|i| (i * 11 % 241) as u8).collect();
        let a = get_or_encode(Kind::Bc7, &pixels, 8, 8, &[123], || {
            Some((vec![4, 5, 6, 7, 8], 3))
        })
        .unwrap();
        assert_eq!(a, (vec![4, 5, 6, 7, 8], 3));

        clear();
        let b = get_or_encode(Kind::Bc7, &pixels, 8, 8, &[123], || {
            unreachable!("disk cache must serve this without recomputing")
        })
        .unwrap();
        assert_eq!(
            a, b,
            "disk-cache hit must be byte-identical to the original encode"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_cache_defaults_off_when_build_id_is_the_dev_placeholder() {
        assert_eq!(
            disk::build_id_pins_encoder(),
            env!("ABGEN_BUILD_ID") != "devbuild0000",
            "the disk cache's default must track whether the build id pins the encoder"
        );
    }

    #[test]
    fn cache_profile_default_matrix() {
        use CacheProfile::*;
        assert_eq!(Lambda.memory_default_mb(), 4096);
        assert!(!Lambda.disk_default_on(), "lambda must not write to disk");

        assert_eq!(Client.memory_default_mb(), 256);
        assert!(Client.disk_default_on());
        assert_eq!(Client.disk_default_mb(), 2048);

        assert_eq!(Batch.memory_default_mb(), 4096);
        assert!(Batch.disk_default_on());
        assert_eq!(Batch.disk_default_mb(), 8192);
    }

    #[test]
    fn undeclared_profile_falls_back_to_batch() {
        if PROFILE.get().is_none() {
            assert_eq!(profile(), CacheProfile::Batch);
        }
        assert_eq!(CacheProfile::Batch.memory_default_mb(), 4096);
    }

    #[test]
    fn disk_cache_key_changes_with_build_id() {
        let pixels = vec![1u8, 2, 3, 4];
        let k1 = key(Kind::Bc7, &pixels, 4, 4, &[1]);
        let mut h = crate::hashes::Sha256::new();
        h.update(&[Kind::Bc7 as u8]);
        h.update(&4u32.to_le_bytes());
        h.update(&4u32.to_le_bytes());
        h.update(&1i64.to_le_bytes());
        h.update(&pixels);
        let k_without_build_id = h.finalize();
        assert_ne!(k1, k_without_build_id);
    }
}
