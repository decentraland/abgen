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

static FORCED: AtomicBool = AtomicBool::new(false);

fn env_enabled() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("ABGEN_TEX_ENCODE_CACHE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

fn max_bytes() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("ABGEN_TEX_ENCODE_CACHE_MAX_MB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(4096)
            .saturating_mul(1024 * 1024)
    })
}

pub fn enable() {
    FORCED.store(true, Ordering::Relaxed);
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
    // Fold in the encoder build id so a stale on-disk entry from a previous
    // build can never serve: bumping ABGEN_BUILD_ID (a content hash of the
    // source tree) or the crate version changes every key.
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

/// Cross-run, on-disk backing for the in-memory encode cache above.
///
/// Same key space (content hash of source pixels + encode params + the
/// encoder's build id, see [`key`]), same value (the encoded block payload),
/// so a disk hit is byte-identical to a fresh encode by construction — it
/// *is* a previous encode's output, not a recomputation. Layout: a shard
/// dir per key's first two hex chars (keeps directories small), one file
/// per key, written tmp-then-renamed so a reader never observes a partial
/// write. Bounded by total bytes with LRU-by-mtime eviction, swept
/// probabilistically (not on every write — a full directory walk per write
/// would undercut the point of the cache) after a successful insert.
///
/// Default on for any build carrying a real content-addressed
/// `ABGEN_BUILD_ID` — i.e. everything `flake.nix`/`release.yml` produce
/// (`ABGEN_DISK_CACHE=0` is the escape hatch). Default *off* for dev builds,
/// whose fixed `devbuild0000` stamp does not pin the encoder, which also
/// keeps `cargo test` (unit and integration alike) off a developer's real
/// cache directory. Tests that exercise it opt back in explicitly and point
/// `ABGEN_DISK_CACHE_DIR` at a throwaway directory.
#[cfg(not(target_arch = "wasm32"))]
mod disk {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    /// Only run the (directory-walking) eviction sweep once every this many
    /// writes: bounds the amortized cost of enforcing the byte budget
    /// without a persistent index, at the price of letting the cache
    /// overshoot the budget by a few entries between sweeps.
    const EVICT_EVERY_N_WRITES: u64 = 8;

    static WRITE_COUNT: AtomicU64 = AtomicU64::new(0);

    /// The placeholder `build.rs` stamps when `ABGEN_BUILD_ID` is unset. It
    /// is a fixed string, not a content id, so it does *not* pin the
    /// encoder: every working tree that builds without an explicit build id
    /// stamps `devbuild0000` and would share one on-disk key space.
    const DEV_BUILD_ID: &str = "devbuild0000";

    /// True when this binary carries a real content-addressed build id.
    /// Every build whose output ships has one — `flake.nix` and
    /// `release.yml` both stamp `nix eval --raw .#buildId` — so the fast
    /// path is still the default everywhere it matters.
    pub(super) fn build_id_pins_encoder() -> bool {
        env!("ABGEN_BUILD_ID") != DEV_BUILD_ID
    }

    fn enabled() -> bool {
        // Default on only where the build id actually pins the encoder.
        // `build.rs` stamps every un-stamped build `devbuild0000`, so all
        // locally built binaries — `abgen-host`, the `live` server, the
        // bench — share one key space no matter what the source tree says.
        // A developer who edits the encoder and rebuilds would then get the
        // *previous* build's bytes served straight off disk, which is the
        // exact "stale build can never serve a hit" invariant this key is
        // supposed to provide. `cfg!(test)` cannot express this either:
        // `crate/tests/*.rs` link this library compiled *without*
        // `cfg(test)`. Keying off the build id covers every case at once.
        // `ABGEN_DISK_CACHE=1` opts back in explicitly.
        crate::clihelp::env_bool("ABGEN_DISK_CACHE", build_id_pins_encoder())
    }

    fn max_bytes() -> u64 {
        std::env::var("ABGEN_DISK_CACHE_MAX_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(8192)
            .saturating_mul(1024 * 1024)
    }

    /// `$ABGEN_DISK_CACHE_DIR`, else `$XDG_CACHE_HOME/abgen`, else the
    /// platform default (`~/Library/Caches/abgen` on macOS, `~/.cache/abgen`
    /// elsewhere) — no new dependency, just the env vars every platform
    /// cache-dir crate reads under the hood. `None` if none of that
    /// resolves (e.g. `$HOME` unset), in which case the disk cache is
    /// silently skipped and callers fall back to memory-only behavior.
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

    /// Walk every shard dir and collect `(path, len, mtime)` for real
    /// entries (skips in-flight `.tmp.` files from a concurrent writer).
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

    /// Evict oldest-by-mtime entries until the shard tree fits `budget`.
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

    /// `None` on any miss or error (missing file, disabled, unresolvable
    /// cache dir, truncated entry) — every case just falls back to a real
    /// encode, so this never needs to distinguish them for callers.
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
        // Touch mtime on read so hot entries survive LRU eviction; best
        // effort, a failure here just makes this entry a slightly earlier
        // eviction candidate, never wrong output.
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
            return; // a single entry bigger than the whole budget: skip it
        }
        let Some(root) = cache_root() else {
            return;
        };
        let path = shard_path(&root, &hex(key));
        if path.exists() {
            return; // content-addressed and immutable: nothing to update
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

/// Evict least-recently-used entries until `incoming` fits under `budget`.
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

/// Like `get_or_encode`, but hands back the cache's own buffer: hits and
/// stored misses cost an `Arc` clone instead of copying the encoded chain.
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
    if let Some((data, mips)) = disk::get(&k) {
        let data = Arc::new(data);
        remember(k, Arc::clone(&data), mips);
        return Some((data, mips));
    }
    let (data, mips) = f()?;
    disk::put(&k, &data, mips);
    let data = Arc::new(data);
    remember(k, Arc::clone(&data), mips);
    Some((data, mips))
}

/// Insert `data`/`mips` into the in-memory map under `k`, respecting the
/// byte budget and LRU eviction. No-op if the key already made it in (e.g.
/// a racing insert) or the entry alone would exceed the whole budget.
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
        let pixels: Vec<u8> = (0..8u32 * 8 * 4).map(|i| (i * 7 % 251) as u8).collect();
        let mut calls = 0u32;
        let a = get_or_encode(Kind::Bc7, &pixels, 8, 8, &[42], || {
            calls += 1;
            Some((vec![1, 2, 3], 4))
        })
        .unwrap();
        let b = get_or_encode(Kind::Bc7, &pixels, 8, 8, &[42], || {
            calls += 1;
            Some((vec![9, 9, 9], 9))
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(a, b);
        assert_eq!(a.0, vec![1, 2, 3]);

        let c = get_or_encode(Kind::Bc7, &pixels, 8, 8, &[43], || Some((vec![5], 1))).unwrap();
        assert_eq!(c.0, vec![5]);

        let d = get_or_encode(Kind::Dxt1, &pixels, 8, 8, &[1], || None);
        assert!(d.is_none());
        let e = get_or_encode(Kind::Dxt1, &pixels, 8, 8, &[1], || Some((vec![7], 1))).unwrap();
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

    /// RAII guard that restores an env var to unset on drop, even on panic
    /// unwind — keeps the disk-cache opt-in tests from leaking state (or a
    /// throwaway directory pointer) into whatever test runs next.
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

        // A real second process would start with an empty in-memory map but
        // the same on-disk cache dir; `clear()` simulates exactly that.
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

    /// Under a dev build the disk cache must default off: `devbuild0000` is
    /// a fixed stamp, so an entry written by one source tree is
    /// indistinguishable from one written by another, and a cross-run hit
    /// would serve a previous build's bytes.
    ///
    /// Asserted on the pure predicate rather than `disk::enabled()`, because
    /// `enabled()` reads a process-global env var that the opt-in test above
    /// concurrently sets.
    #[test]
    fn disk_cache_defaults_off_when_build_id_is_the_dev_placeholder() {
        assert_eq!(
            disk::build_id_pins_encoder(),
            env!("ABGEN_BUILD_ID") != "devbuild0000",
            "the disk cache's default must track whether the build id pins the encoder"
        );
    }

    #[test]
    fn disk_cache_key_changes_with_build_id() {
        // The key folds in CARGO_PKG_VERSION + ABGEN_BUILD_ID; two encodes
        // that only differ if the build id differed would need separate
        // entries. We can't rebuild with a different id in a unit test, but
        // we can assert the key function actually mixes both in (i.e. it
        // isn't silently dead code the optimizer could drop).
        let pixels = vec![1u8, 2, 3, 4];
        let k1 = key(Kind::Bc7, &pixels, 4, 4, &[1]);
        // A hand-rolled hash that omits build id/version must differ.
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
