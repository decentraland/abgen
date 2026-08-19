//! Process-wide memo cache for compressed-texture encode results.
//!
//! Texture encoding (BC7 in particular) dominates conversion CPU time and is
//! fully platform-independent: building the same entity for `windows` and
//! `mac` encodes identical pixels into identical blocks twice. With this
//! cache enabled, the second target reuses the first target's encodes, so a
//! dual-target build costs roughly one target plus serialization.
//!
//! Disabled by default: a long-running server would grow without bound and
//! single-target builds gain nothing. Opt in with `ABGEN_TEX_ENCODE_CACHE=1`
//! or [`enable`]. Callers that loop over entities should [`clear`] between
//! entities. Inserts stop once resident encoded bytes would exceed
//! `ABGEN_TEX_ENCODE_CACHE_MAX_MB` (default 4096); lookups keep working.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy)]
pub enum Kind {
    Bc7 = 1,
    Dxt1 = 2,
    Dxt5Crn = 3,
    Bc3 = 4,
}

struct Store {
    map: HashMap<[u8; 32], (Vec<u8>, i32)>,
    bytes: usize,
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
            hits: 0,
            misses: 0,
        })
    })
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

/// Returns the cached encode for `(kind, pixels, dims, params)` or runs `f`
/// and caches its result. `None` results are never cached. The lock is not
/// held while `f` runs, so two threads racing on the same texture may both
/// encode it; the winner's insert stands.
pub fn get_or_encode(
    kind: Kind,
    pixels: &[u8],
    width: u32,
    height: u32,
    params: &[i64],
    f: impl FnOnce() -> Option<(Vec<u8>, i32)>,
) -> Option<(Vec<u8>, i32)> {
    if !enabled() {
        return f();
    }
    let k = key(kind, pixels, width, height, params);
    {
        let mut s = store().lock().unwrap();
        if let Some((data, mips)) = s.map.get(&k) {
            let out = (data.clone(), *mips);
            s.hits += 1;
            return Some(out);
        }
        s.misses += 1;
    }
    let result = f()?;
    let len = result.0.len();
    let mut s = store().lock().unwrap();
    if s.bytes + len <= max_bytes() {
        if let std::collections::hash_map::Entry::Vacant(e) = s.map.entry(k) {
            e.insert((result.0.clone(), result.1));
            s.bytes += len;
        }
    }
    Some(result)
}

/// `(hits, misses, resident bytes, entries)`
pub fn stats() -> (u64, u64, usize, usize) {
    let s = store().lock().unwrap();
    (s.hits, s.misses, s.bytes, s.map.len())
}

pub fn clear() {
    let mut s = store().lock().unwrap();
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
}
