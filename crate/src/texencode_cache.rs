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

fn key(kind: Kind, pixels: &[u8], width: u32, height: u32, params: &[i64]) -> [u8; 32] {
    let mut h = crate::hashes::Sha256::new();
    h.update(&[kind as u8]);
    h.update(&width.to_le_bytes());
    h.update(&height.to_le_bytes());
    for p in params {
        h.update(&p.to_le_bytes());
    }
    h.update(pixels);
    h.finalize()
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
    let (data, mips) = f()?;
    let len = data.len();
    let data = Arc::new(data);
    let budget = max_bytes();
    if len <= budget {
        let mut s = lock();
        s.stamp += 1;
        let stamp = s.stamp;
        if !s.map.contains_key(&k) {
            make_room(&mut s, len, budget);
            s.map.insert(k, (Arc::clone(&data), mips, stamp));
            s.bytes += len;
        }
    }
    Some((data, mips))
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
}
