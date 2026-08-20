//! Decoded-source-image reuse across the per-entity platform loop.
//!
//! The encode cache already dedupes the *encode* of identical pixels, so on
//! the second platform of a conversion the remaining repeated cost is
//! decoding every source image again. This cache keys the decoded RGBA by
//! the source bytes' digest and hands out shared buffers, bounded by bytes
//! and cleared per entity by the same guard that clears the encode cache.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use image::RgbaImage;

struct Store {
    map: HashMap<[u8; 32], (Arc<RgbaImage>, u64)>,
    bytes: usize,
    stamp: u64,
    hits: u64,
    misses: u64,
}

static FORCED: AtomicBool = AtomicBool::new(false);

fn max_bytes() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("ABGEN_DECODE_CACHE_MB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(512)
            .saturating_mul(1024 * 1024)
    })
}

pub fn enable() {
    FORCED.store(true, Ordering::Relaxed);
}

fn enabled() -> bool {
    FORCED.load(Ordering::Relaxed) && max_bytes() > 0
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

/// Evict least-recently-used entries until `incoming` fits under `budget`.
fn make_room(s: &mut Store, incoming: usize, budget: usize) {
    while s.bytes + incoming > budget && !s.map.is_empty() {
        let oldest = s
            .map
            .iter()
            .min_by_key(|(_, (_, stamp))| *stamp)
            .map(|(k, _)| *k)
            .expect("non-empty map has a minimum");
        if let Some((img, _)) = s.map.remove(&oldest) {
            s.bytes -= img.as_raw().len();
        }
    }
}

pub fn get_or_decode(raw: &[u8], f: impl FnOnce() -> Option<RgbaImage>) -> Option<Arc<RgbaImage>> {
    if !enabled() {
        return f().map(Arc::new);
    }
    let k = crate::hashes::sha256(raw);
    {
        let mut s = lock();
        s.stamp += 1;
        let stamp = s.stamp;
        if let Some((img, at)) = s.map.get_mut(&k) {
            *at = stamp;
            let out = Arc::clone(img);
            s.hits += 1;
            return Some(out);
        }
        s.misses += 1;
    }
    let img = Arc::new(f()?);
    let len = img.as_raw().len();
    let budget = max_bytes();
    if len <= budget {
        let mut s = lock();
        s.stamp += 1;
        let stamp = s.stamp;
        if !s.map.contains_key(&k) {
            make_room(&mut s, len, budget);
            s.map.insert(k, (Arc::clone(&img), stamp));
            s.bytes += len;
        }
    }
    Some(img)
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

    fn img(w: u32, h: u32, seed: u8) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| image::Rgba([seed, x as u8, y as u8, 255]))
    }

    #[test]
    fn hit_shares_the_stored_image() {
        enable();
        clear();
        let raw = b"decode-cache-test-input-1".to_vec();
        let mut calls = 0u32;
        let a = get_or_decode(&raw, || {
            calls += 1;
            Some(img(4, 4, 1))
        })
        .unwrap();
        let b = get_or_decode(&raw, || {
            calls += 1;
            Some(img(4, 4, 2))
        })
        .unwrap();
        assert_eq!(calls, 1, "second lookup must be a hit");
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.as_raw(), b.as_raw());

        let miss = get_or_decode(b"other-input", || None);
        assert!(miss.is_none());
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
        let budget = 3 * 16 * 16 * 4;
        for seed in 0u8..3 {
            s.stamp += 1;
            let i = Arc::new(img(16, 16, seed));
            s.bytes += i.as_raw().len();
            s.map.insert([seed; 32], (i, s.stamp));
        }
        // Touch entry 0 so entry 1 becomes the LRU victim.
        s.stamp += 1;
        let stamp = s.stamp;
        s.map.get_mut(&[0u8; 32]).unwrap().1 = stamp;

        make_room(&mut s, 16 * 16 * 4, budget);
        assert!(s.map.contains_key(&[0u8; 32]));
        assert!(!s.map.contains_key(&[1u8; 32]), "LRU entry evicted");
        assert!(s.map.contains_key(&[2u8; 32]));
        assert_eq!(s.bytes, 2 * 16 * 16 * 4);
    }
}
