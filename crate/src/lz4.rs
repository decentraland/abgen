use std::cell::RefCell;
use std::fmt;

const MINMATCH: i32 = 4;
const LASTLITERALS: usize = 5;
const MFLIMIT: usize = 12;
const ML_BITS: u32 = 4;
const ML_MASK: u32 = (1 << ML_BITS) - 1;
const RUN_BITS: u32 = 8 - ML_BITS;
const RUN_MASK: u32 = (1 << RUN_BITS) - 1;
const LZ4_MINLENGTH: usize = MFLIMIT + 1;
const LZ4_DISTANCE_MAX: u32 = 65535;

const LZ4HC_HASH_LOG: u32 = 15;
const LZ4HC_HASHTABLESIZE: usize = 1 << LZ4HC_HASH_LOG;
const LZ4HC_DICTIONARY_LOGSIZE: u32 = 16;
const LZ4HC_MAXD: usize = 1 << LZ4HC_DICTIONARY_LOGSIZE;

const LZ4_OPT_NUM: usize = 1 << 12;
const TRAILING_LITERALS: usize = 3;

const NB_SEARCHES: i32 = 96;
const SUFFICIENT_LEN: usize = 64;
const _: () = assert!(SUFFICIENT_LEN < LZ4_OPT_NUM);

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Lz4Error {
    Malformed(&'static str),
}

impl fmt::Display for Lz4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Lz4Error::Malformed(m) => write!(f, "lz4 decompress: {m}"),
        }
    }
}

impl std::error::Error for Lz4Error {}

const MAX_DECOMPRESS_BYTES: usize = 256 * 1024 * 1024;

/// Match copies may overshoot the match end by up to 7 bytes; the buffer
/// carries this much slack past `dst_size` so those stores stay in bounds.
/// Every byte of `[0, dst_size)` is still written exactly by some later
/// literal or match (`dp` advances contiguously to `dst_size`), so an
/// overshoot never survives into the returned bytes.
const WILDCOPY_SLACK: usize = 8;

pub fn decompress(src: &[u8], dst_size: usize) -> Result<Vec<u8>, Lz4Error> {
    if dst_size > MAX_DECOMPRESS_BYTES {
        return Err(Lz4Error::Malformed(
            "decompressed size exceeds MAX_DECOMPRESS_BYTES",
        ));
    }
    let mut dst = vec![0u8; dst_size + WILDCOPY_SLACK];
    let mut sp = 0usize;
    let mut dp = 0usize;
    let slen = src.len();

    if slen == 0 {
        if dst_size == 0 {
            dst.clear();
            return Ok(dst);
        }
        return Err(Lz4Error::Malformed("empty input but non-zero output"));
    }

    loop {
        if sp >= slen {
            return Err(Lz4Error::Malformed("truncated token"));
        }
        let token = src[sp] as u32;
        sp += 1;

        let mut lit_len = (token >> ML_BITS) as usize;
        if lit_len == RUN_MASK as usize {
            loop {
                if sp >= slen {
                    return Err(Lz4Error::Malformed("truncated literal length"));
                }
                let b = src[sp];
                sp += 1;
                lit_len += b as usize;
                if b != 255 {
                    break;
                }
            }
        }

        if lit_len > 0 {
            if sp + lit_len > slen {
                return Err(Lz4Error::Malformed("literal run exceeds input"));
            }
            if dp + lit_len > dst_size {
                return Err(Lz4Error::Malformed("literal run exceeds output"));
            }
            dst[dp..dp + lit_len].copy_from_slice(&src[sp..sp + lit_len]);
            sp += lit_len;
            dp += lit_len;
        }

        if sp == slen {
            break;
        }

        if sp + 2 > slen {
            return Err(Lz4Error::Malformed("truncated offset"));
        }
        let offset = (src[sp] as usize) | ((src[sp + 1] as usize) << 8);
        sp += 2;
        if offset == 0 {
            return Err(Lz4Error::Malformed("zero offset"));
        }
        if offset > dp {
            return Err(Lz4Error::Malformed("offset before output start"));
        }

        let mut match_len = (token & ML_MASK) as usize;
        if match_len == ML_MASK as usize {
            loop {
                if sp >= slen {
                    return Err(Lz4Error::Malformed("truncated match length"));
                }
                let b = src[sp];
                sp += 1;
                match_len += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        match_len += MINMATCH as usize;

        if dp + match_len > dst_size {
            return Err(Lz4Error::Malformed("match copy exceeds output"));
        }

        let mp = dp - offset;
        if offset >= 8 {
            // Chunks of the source never overlap their destination at this
            // offset, so 8-byte copies preserve byte-loop semantics.
            let mut s = mp;
            let mut d = dp;
            let end = dp + match_len;
            while d < end {
                unsafe {
                    let v = (dst.as_ptr().add(s) as *const u64).read_unaligned();
                    (dst.as_mut_ptr().add(d) as *mut u64).write_unaligned(v);
                }
                s += 8;
                d += 8;
            }
        } else if offset == 1 || offset == 2 || offset == 4 {
            // 8 is a multiple of the offset, so a repeated 8-byte pattern
            // keeps the phase a byte loop would produce.
            let pat64 = match offset {
                1 => 0x0101010101010101u64.wrapping_mul(dst[mp] as u64),
                2 => {
                    let p = (dst[mp] as u64) | ((dst[mp + 1] as u64) << 8);
                    p | (p << 16) | (p << 32) | (p << 48)
                }
                _ => {
                    let p = (dst[mp] as u64)
                        | ((dst[mp + 1] as u64) << 8)
                        | ((dst[mp + 2] as u64) << 16)
                        | ((dst[mp + 3] as u64) << 24);
                    p | (p << 32)
                }
            };
            let mut d = dp;
            let end = dp + match_len;
            while d < end {
                unsafe {
                    (dst.as_mut_ptr().add(d) as *mut u64).write_unaligned(pat64.to_le());
                }
                d += 8;
            }
        } else {
            // Offsets 3, 5, 6, 7 overlap within a word at a phase 8 doesn't
            // preserve; they are rare enough that bytes are fine.
            for k in 0..match_len {
                dst[dp + k] = dst[mp + k];
            }
        }
        dp += match_len;
    }

    if dp != dst_size {
        return Err(Lz4Error::Malformed("decompressed size mismatch"));
    }
    dst.truncate(dst_size);
    Ok(dst)
}

#[inline(always)]
fn read32(buf: &[u8], i: usize) -> u32 {
    debug_assert!(i + 4 <= buf.len());

    unsafe { (buf.as_ptr().add(i) as *const u32).read_unaligned() }.to_le()
}

#[inline(always)]
fn read16(buf: &[u8], i: usize) -> u16 {
    debug_assert!(i + 2 <= buf.len());

    unsafe { (buf.as_ptr().add(i) as *const u16).read_unaligned() }.to_le()
}

#[inline(always)]
fn read64(buf: &[u8], i: usize) -> u64 {
    debug_assert!(i + 8 <= buf.len());

    unsafe { (buf.as_ptr().add(i) as *const u64).read_unaligned() }.to_le()
}

#[inline]
fn hash_ptr(buf: &[u8], i: usize) -> u32 {
    let v = read32(buf, i);
    v.wrapping_mul(2654435761)
        .wrapping_shr((MINMATCH as u32 * 8) - LZ4HC_HASH_LOG)
}

#[inline]
fn lz4_count(buf: &[u8], mut pin: usize, mut pmatch: usize, limit: usize) -> u32 {
    let start = pin;
    const STEP: usize = 8;

    debug_assert!(limit <= buf.len());
    while pin + STEP <= limit {
        let a = read64(buf, pin);
        let b = read64(buf, pmatch);
        let diff = a ^ b;
        if diff == 0 {
            pin += STEP;
            pmatch += STEP;
            continue;
        }
        pin += (diff.trailing_zeros() / 8) as usize;
        return (pin - start) as u32;
    }

    if pin < limit {
        debug_assert!(limit >= 8);
        let q = limit - 8;
        let back = pin - q;
        if pmatch >= back {
            let diff = (read64(buf, q) ^ read64(buf, pmatch - back)) >> (8 * back);
            if diff != 0 {
                pin += (diff.trailing_zeros() / 8) as usize;
            } else {
                pin = limit;
            }
        } else {
            while pin < limit && buf[pin] == buf[pmatch] {
                pin += 1;
                pmatch += 1;
            }
        }
    }
    (pin - start) as u32
}

#[inline]
fn count_back(buf: &[u8], ip: usize, m: usize, imin: usize, mmin: usize) -> i32 {
    let mut back: i32 = 0;

    let min = std::cmp::max(imin as i64 - ip as i64, mmin as i64 - m as i64) as i32;
    while (back > min)
        && buf[(ip as i32 + back - 1) as usize] == buf[(m as i32 + back - 1) as usize]
    {
        back -= 1;
    }
    back
}

struct HcCtx {
    hash_table: Box<[u32; LZ4HC_HASHTABLESIZE]>,

    combo_table: Box<[u32; LZ4HC_MAXD]>,

    base: u32,
    next_to_update: u32,

    /// One past the highest index the tables may still refer to; the next
    /// `prepare` rebases beyond it.
    hi: u32,
}

impl HcCtx {
    fn new() -> Self {
        let base = 64 * 1024;
        let hash_table: Box<[u32; LZ4HC_HASHTABLESIZE]> = vec![0u32; LZ4HC_HASHTABLESIZE]
            .into_boxed_slice()
            .try_into()
            .unwrap_or_else(|_| unreachable!());

        let combo_table: Box<[u32; LZ4HC_MAXD]> = vec![0x0000FFFFu32; LZ4HC_MAXD]
            .into_boxed_slice()
            .try_into()
            .unwrap_or_else(|_| unreachable!());
        HcCtx {
            hash_table,
            combo_table,
            base,
            next_to_update: base,
            hi: base,
        }
    }

    /// Make the context safe for a fresh buffer without clearing the tables.
    ///
    /// Rebasing `base` at least 64 KiB past every index the tables may still
    /// hold keeps stale entries indistinguishable from a zeroed table: a
    /// stale `hash_table` hit always yields `delta > LZ4_DISTANCE_MAX`,
    /// which `insert` clamps and whose `sum` saturates to 0xFFFF — the exact
    /// values the zero entries of a fresh table produce (`base` starts at
    /// 64 KiB for the same reason) — and the search loops reject any stale
    /// candidate on the `lowest_match_index` bound before dereferencing it.
    /// Match discovery, and therefore compressed output, is bit-identical to
    /// a fresh context (`hc_ctx_reuse_matches_fresh` pins this); the saving
    /// is a 384 KiB memset per compressed chunk plus the cache eviction it
    /// causes. The index space is u32, so once the headroom is spent the
    /// tables are cleared for real and `base` starts over.
    fn prepare(&mut self, src_len: usize) {
        let rebased = self.hi as u64 + 64 * 1024;
        if rebased + src_len as u64 > u32::MAX as u64 - 64 * 1024 {
            self.hash_table.fill(0);
            self.combo_table.fill(0x0000FFFFu32);
            self.base = 64 * 1024;
        } else {
            self.base = rebased as u32;
        }
        self.next_to_update = self.base;
    }

    fn insert(&mut self, buf: &[u8], ip: usize) {
        let prefix_idx = self.base;
        let target = ip as u32 + prefix_idx;
        let mut idx = self.next_to_update;
        let pending = target.saturating_sub(idx);

        const HASH_BATCH: usize = 8;
        if pending as usize >= HASH_BATCH && buf.len() >= 4 + HASH_BATCH {
            let mut hashes = [0u32; HASH_BATCH];
            while idx + HASH_BATCH as u32 <= target {
                let pos0 = (idx - prefix_idx) as usize;
                if pos0 + 4 + HASH_BATCH > buf.len() {
                    break;
                }
                hash_batch_8(buf, pos0, &mut hashes);
                for k in 0..HASH_BATCH {
                    let h = (hashes[k] as usize) & (LZ4HC_HASHTABLESIZE - 1);
                    let cur = idx + k as u32;
                    let mut delta = cur - self.hash_table[h];
                    if delta > LZ4_DISTANCE_MAX {
                        delta = LZ4_DISTANCE_MAX;
                    }
                    let d2 = self.combo_table[(cur.wrapping_sub(delta) & 0xFFFF) as usize] & 0xFFFF;
                    let sum = (delta + d2).min(0xFFFF);
                    self.combo_table[(cur & 0xFFFF) as usize] = delta | (sum << 16);
                    self.hash_table[h] = cur;
                }
                idx += HASH_BATCH as u32;
            }
        }

        while idx < target {
            let pos = (idx - prefix_idx) as usize;
            let h = (hash_ptr(buf, pos) as usize) & (LZ4HC_HASHTABLESIZE - 1);
            let mut delta = idx - self.hash_table[h];
            if delta > LZ4_DISTANCE_MAX {
                delta = LZ4_DISTANCE_MAX;
            }
            let d2 = self.combo_table[(idx.wrapping_sub(delta) & 0xFFFF) as usize] & 0xFFFF;
            let sum = (delta + d2).min(0xFFFF);
            self.combo_table[(idx & 0xFFFF) as usize] = delta | (sum << 16);
            self.hash_table[h] = idx;
            idx += 1;
        }
        self.next_to_update = target;
    }
}

thread_local! {
    static HC_CTX_POOL: RefCell<HcCtx> = RefCell::new(HcCtx::new());
}

#[cfg(target_arch = "x86_64")]
fn has_avx2() -> bool {
    use std::sync::OnceLock;
    static HAS_AVX2: OnceLock<bool> = OnceLock::new();
    *HAS_AVX2.get_or_init(|| {
        if std::env::var_os("ABGEN_LZ4_SCALAR").is_some() {
            return false;
        }
        std::is_x86_feature_detected!("avx2")
    })
}

/// Same layout trick as the NEON path: two overlapping 8-byte loads pack
/// bytes [0..8) and [4..12) into one 16-byte vector, then per-128-lane
/// shuffles expand the 8 overlapping little-endian u32 words.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn hash_batch_8_avx2(buf: &[u8], pos0: usize, out: &mut [u32; 8]) {
    use std::arch::x86_64::*;
    debug_assert!(pos0 + 12 <= buf.len());
    const SHR: i32 = (MINMATCH * 8) - LZ4HC_HASH_LOG as i32;
    let lo = _mm_loadl_epi64(buf.as_ptr().add(pos0) as *const __m128i);
    let hi = _mm_loadl_epi64(buf.as_ptr().add(pos0 + 4) as *const __m128i);
    let bytes = _mm_unpacklo_epi64(lo, hi);
    // Bytes 8..12 live in lanes 12..16 of the combined vector.
    const IDX: [u8; 32] = [
        0, 1, 2, 3, 1, 2, 3, 4, 2, 3, 4, 5, 3, 4, 5, 6, // words 0..4
        4, 5, 6, 7, 5, 6, 7, 12, 6, 7, 12, 13, 7, 12, 13, 14, // words 4..8
    ];
    let dup = _mm256_broadcastsi128_si256(bytes);
    let idx = _mm256_loadu_si256(IDX.as_ptr() as *const __m256i);
    let words = _mm256_shuffle_epi8(dup, idx);
    let prime = _mm256_set1_epi32(2654435761u32 as i32);
    let hashed = _mm256_srli_epi32::<SHR>(_mm256_mullo_epi32(words, prime));
    _mm256_storeu_si256(out.as_mut_ptr() as *mut __m256i, hashed);
}

#[inline(always)]
fn hash_batch_8(buf: &[u8], pos0: usize, out: &mut [u32; 8]) {
    #[cfg(target_arch = "x86_64")]
    if has_avx2() {
        unsafe { hash_batch_8_avx2(buf, pos0, out) };
        return;
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        use std::arch::aarch64::*;
        debug_assert!(pos0 + 12 <= buf.len());
        const SHR: i32 = (MINMATCH * 8) - LZ4HC_HASH_LOG as i32;
        let lo = vld1_u8(buf.as_ptr().add(pos0));
        let hi = vld1_u8(buf.as_ptr().add(pos0 + 4));
        let bytes = vcombine_u8(lo, hi);
        const IDX_LO: [u8; 16] = [0, 1, 2, 3, 1, 2, 3, 4, 2, 3, 4, 5, 3, 4, 5, 6];
        const IDX_HI: [u8; 16] = [4, 5, 6, 7, 5, 6, 7, 12, 6, 7, 12, 13, 7, 12, 13, 14];
        let vlo = vreinterpretq_u32_u8(vqtbl1q_u8(bytes, vld1q_u8(IDX_LO.as_ptr())));
        let vhi = vreinterpretq_u32_u8(vqtbl1q_u8(bytes, vld1q_u8(IDX_HI.as_ptr())));
        let prime = vdupq_n_u32(2654435761);
        let h0 = vshrq_n_u32::<SHR>(vmulq_u32(vlo, prime));
        let h1 = vshrq_n_u32::<SHR>(vmulq_u32(vhi, prime));
        vst1q_u32(out.as_mut_ptr(), h0);
        vst1q_u32(out.as_mut_ptr().add(4), h1);
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        for k in 0..8 {
            out[k] = hash_ptr(buf, pos0 + k);
        }
    }
}

#[inline(always)]
fn delta_next_u16(combo: &[u32; LZ4HC_MAXD], pos: u32) -> u32 {
    combo[(pos & 0xFFFF) as usize] & 0xFFFF
}

fn count_pattern(buf: &[u8], mut ip: usize, iend: usize, pattern32: u32) -> u32 {
    let start = ip;

    let pattern64 = (pattern32 as u64) | ((pattern32 as u64) << 32);
    while ip + 8 <= iend {
        let diff = read64(buf, ip) ^ pattern64;
        if diff != 0 {
            ip += (diff.trailing_zeros() / 8) as usize;
            return (ip - start) as u32;
        }
        ip += 8;
    }
    if ip < iend {
        debug_assert!(iend >= 8);
        let q = iend - 8;
        let phase = (q.wrapping_sub(start) & 3) as u32;
        let exp32 = pattern32.rotate_right(8 * phase);
        let exp64 = (exp32 as u64) | ((exp32 as u64) << 32);
        let diff = (read64(buf, q) ^ exp64) >> (8 * (ip - q));
        if diff != 0 {
            ip += (diff.trailing_zeros() / 8) as usize;
        } else {
            ip = iend;
        }
    }
    (ip - start) as u32
}

fn reverse_count_pattern(buf: &[u8], mut ip: usize, ilow: usize, pattern: u32) -> u32 {
    let start = ip;

    let pattern64 = (pattern as u64) | ((pattern as u64) << 32);
    while ip >= ilow + 8 {
        let m = read64(buf, ip - 8) ^ pattern64;
        if m != 0 {
            return (start - ip) as u32 + m.leading_zeros() / 8;
        }
        ip -= 8;
    }
    while ip >= ilow + 4 {
        let m = read32(buf, ip - 4) ^ pattern;
        if m != 0 {
            return (start - ip) as u32 + m.leading_zeros() / 8;
        }
        ip -= 4;
    }

    let rem = ip - ilow;
    if rem > 0 {
        debug_assert!(ilow + 4 <= buf.len());
        let exp = pattern.rotate_right(8 * ((ilow.wrapping_sub(ip) & 3) as u32));
        let diff = (read32(buf, ilow) ^ exp) << (8 * (4 - rem));
        let matched = if diff == 0 {
            rem as u32
        } else {
            (diff.leading_zeros() / 8).min(rem as u32)
        };
        return (start - ip) as u32 + matched;
    }
    (start - ip) as u32
}

#[inline]
const fn protect_dict_end(dict_limit: u32, match_index: u32) -> bool {
    (dict_limit.wrapping_sub(1).wrapping_sub(match_index)) >= 3
}

#[derive(Clone, Copy, Default)]
struct Match {
    off: i32,
    len: i32,
}

#[derive(PartialEq, Clone, Copy)]
enum RepeatState {
    Untested,
    Not,
    Confirmed,
}

#[inline(never)]
fn chain_swap_scan(combo_table: &[u32; LZ4HC_MAXD], match_index: u32, longest: i32) -> (u32, u32) {
    let k_trigger = 4;
    let mut distance_to_next_match: u32 = 1;
    let mut match_chain_pos: u32 = 0;
    let end = longest - MINMATCH + 1;
    let mut step;
    let mut accel = 1i32 << k_trigger;
    let mut pos = 0i32;
    while pos < end {
        let candidate_dist = delta_next_u16(combo_table, match_index + pos as u32);
        step = accel >> k_trigger;
        accel += 1;
        if candidate_dist > distance_to_next_match {
            distance_to_next_match = candidate_dist;
            match_chain_pos = pos as u32;
            accel = 1 << k_trigger;
        }
        pos += step;
    }
    (distance_to_next_match, match_chain_pos)
}

#[derive(Default)]
struct PatternMemo {
    s: [usize; 2],
    e: [usize; 2],
}

impl PatternMemo {
    #[inline]
    fn query(&self, p: usize) -> Option<(usize, usize)> {
        for k in 0..2 {
            if self.s[k] <= p && p + 4 <= self.e[k] {
                return Some((self.e[k] - p, p - self.s[k]));
            }
        }
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn insert_and_get_wider_match(
    ctx: &mut HcCtx,
    buf: &[u8],
    ip: usize,
    ilow_limit: usize,
    ihigh_limit: usize,
    mut longest: i32,
    max_nb_attempts: i32,
    pattern_analysis: bool,
    chain_swap: bool,
    memo: &mut PatternMemo,
) -> Match {
    let prefix_idx = ctx.base;
    let ip_index = ip as u32 + prefix_idx;
    let within_start_distance = (ctx.base + (LZ4_DISTANCE_MAX + 1)) > ip_index;
    let lowest_match_index = if within_start_distance {
        ctx.base
    } else {
        ip_index - LZ4_DISTANCE_MAX
    };
    let look_back_length = (ip - ilow_limit) as i32;
    let mut nb_attempts = max_nb_attempts;
    let mut match_chain_pos: u32 = 0;
    let pattern = read32(buf, ip);
    let mut repeat = RepeatState::Untested;
    let mut src_pattern_length: usize = 0;
    let mut offset = 0i32;

    ctx.insert(buf, ip);
    let mut match_index = ctx.hash_table[(hash_ptr(buf, ip) as usize) & (LZ4HC_HASHTABLESIZE - 1)];

    let mut lo16 = read16(buf, (ilow_limit as isize + longest as isize - 1) as usize);
    let mut mo_add = longest as isize - 1 - look_back_length as isize;

    let mut to_phase2 = false;

    macro_rules! probe {
        () => {{
            let mut match_length: i32 = 0;
            debug_assert!(match_index < ip_index);
            debug_assert!(match_index >= prefix_idx);
            let match_ptr = (match_index - prefix_idx) as usize;
            debug_assert!(match_ptr < ip);
            debug_assert!(longest >= 1);
            let mo = (match_ptr as isize + mo_add) as usize;
            if read16(buf, mo) == lo16 && read32(buf, match_ptr) == pattern {
                let back = if look_back_length != 0 {
                    count_back(buf, ip, match_ptr, ilow_limit, 0)
                } else {
                    0
                };
                match_length = MINMATCH
                    + lz4_count(
                        buf,
                        ip + MINMATCH as usize,
                        match_ptr + MINMATCH as usize,
                        ihigh_limit,
                    ) as i32;
                match_length -= back;
                if match_length > longest {
                    longest = match_length;
                    offset = (ip_index - match_index) as i32;
                    lo16 = read16(buf, (ilow_limit as isize + longest as isize - 1) as usize);
                    mo_add = longest as isize - 1 - look_back_length as isize;
                }
            }
            match_length
        }};
    }

    macro_rules! pattern_path {
        ($walk:lifetime) => {{
            let match_candidate_idx = match_index - 1;
            if repeat == RepeatState::Untested {
                if (pattern & 0xFFFF) == (pattern >> 16) && (pattern & 0xFF) == (pattern >> 24)
                {
                    repeat = RepeatState::Confirmed;
                    src_pattern_length = if let Some((fwd, _)) = memo.query(ip) {
                        fwd
                    } else {
                        let l = count_pattern(buf, ip + 4, ihigh_limit, pattern) as usize + 4;
                        let back = reverse_count_pattern(buf, ip, 0, pattern) as usize;
                        memo.s[0] = ip - back;
                        memo.e[0] = ip + l;
                        l
                    };
                } else {
                    repeat = RepeatState::Not;
                }
            }
            if repeat == RepeatState::Confirmed
                && match_candidate_idx >= lowest_match_index
                && protect_dict_end(prefix_idx, match_candidate_idx)
            {
                let match_ptr = (match_candidate_idx - prefix_idx) as usize;
                if read32(buf, match_ptr) == pattern {
                    let i_limit = ihigh_limit;
                    let (forward_pattern_length, back_raw) =
                        if let Some(hit) = memo.query(match_ptr) {
                            hit
                        } else {
                            let fwd = count_pattern(buf, match_ptr + 4, i_limit, pattern)
                                as usize
                                + 4;
                            let back =
                                reverse_count_pattern(buf, match_ptr, 0, pattern) as usize;
                            memo.s[1] = match_ptr - back;
                            memo.e[1] = match_ptr + fwd;
                            (fwd, back)
                        };

                    {
                        let mut back_length = back_raw;

                        let lower = std::cmp::max(
                            match_candidate_idx.wrapping_sub(back_length as u32),
                            lowest_match_index,
                        );
                        back_length = (match_candidate_idx - lower) as usize;
                        let current_segment_length = back_length + forward_pattern_length;

                        if current_segment_length >= src_pattern_length
                            && forward_pattern_length <= src_pattern_length
                        {
                            let new_match_index = match_candidate_idx
                                + forward_pattern_length as u32
                                - src_pattern_length as u32;
                            if protect_dict_end(prefix_idx, new_match_index) {
                                match_index = new_match_index;
                            } else {
                                match_index = prefix_idx;
                            }
                        } else {
                            let new_match_index = match_candidate_idx - back_length as u32;
                            if !protect_dict_end(prefix_idx, new_match_index) {
                                match_index = prefix_idx;
                            } else {
                                match_index = new_match_index;

                                if look_back_length == 0 {
                                    let max_ml = std::cmp::min(
                                        current_segment_length,
                                        src_pattern_length,
                                    );
                                    if (longest as usize) < max_ml {
                                        if (ip as u32 + prefix_idx - match_index)
                                            > LZ4_DISTANCE_MAX
                                        {
                                            break $walk;
                                        }
                                        longest = max_ml as i32;
                                        offset = (ip_index - match_index) as i32;
                                        lo16 = read16(
                                            buf,
                                            (ilow_limit as isize + longest as isize - 1)
                                                as usize,
                                        );
                                        mo_add =
                                            longest as isize - 1 - look_back_length as isize;
                                    }
                                    let dist_to_next_pattern =
                                        delta_next_u16(&ctx.combo_table, match_index);
                                    if dist_to_next_pattern > match_index {
                                        break $walk;
                                    }
                                    match_index -= dist_to_next_pattern;
                                }
                            }
                        }
                    }
                    continue $walk;
                }
            }
        }};
    }

    'walk: while match_index >= lowest_match_index && nb_attempts > 0 {
        let packed = ctx.combo_table[(match_index & 0xFFFF) as usize];
        let d1 = packed & 0xFFFF;
        let sum = packed >> 16;

        nb_attempts -= 1;
        let ml_a = probe!();
        if chain_swap && ml_a == longest {
            debug_assert!(look_back_length == 0);
            if match_index + longest as u32 <= ip_index {
                let (distance_to_next_match, scan_pos) =
                    chain_swap_scan(&ctx.combo_table, match_index, longest);
                if distance_to_next_match > 1 {
                    match_chain_pos = scan_pos;
                    if distance_to_next_match > match_index {
                        break 'walk;
                    }
                    match_index -= distance_to_next_match;
                    if match_chain_pos == 0 {
                        continue 'walk;
                    }
                    to_phase2 = true;
                    break 'walk;
                }
            }
        }
        if pattern_analysis && d1 == 1 {
            pattern_path!('walk);
        }

        match_index -= d1;
        if match_index < lowest_match_index || nb_attempts <= 0 {
            break 'walk;
        }

        nb_attempts -= 1;
        let d2 = if sum == 0xFFFF {
            delta_next_u16(&ctx.combo_table, match_index)
        } else {
            sum - d1
        };
        let ml_b = probe!();
        if chain_swap && ml_b == longest {
            debug_assert!(look_back_length == 0);
            if match_index + longest as u32 <= ip_index {
                let (distance_to_next_match, scan_pos) =
                    chain_swap_scan(&ctx.combo_table, match_index, longest);
                if distance_to_next_match > 1 {
                    match_chain_pos = scan_pos;
                    if distance_to_next_match > match_index {
                        break 'walk;
                    }
                    match_index -= distance_to_next_match;
                    if match_chain_pos == 0 {
                        continue 'walk;
                    }
                    to_phase2 = true;
                    break 'walk;
                }
            }
        }
        if pattern_analysis && d2 == 1 {
            pattern_path!('walk);
        }
        match_index -= d2;
    }

    if to_phase2 {
        'p2: while match_index >= lowest_match_index && nb_attempts > 0 {
            nb_attempts -= 1;
            let match_length = probe!();

            if chain_swap && match_length == longest {
                debug_assert!(look_back_length == 0);
                if match_index + longest as u32 <= ip_index {
                    let (distance_to_next_match, scan_pos) =
                        chain_swap_scan(&ctx.combo_table, match_index, longest);
                    if distance_to_next_match > 1 {
                        match_chain_pos = scan_pos;
                        if distance_to_next_match > match_index {
                            break 'p2;
                        }
                        match_index -= distance_to_next_match;
                        continue 'p2;
                    }
                }
            }

            let dist_next_match = delta_next_u16(&ctx.combo_table, match_index + match_chain_pos);
            if pattern_analysis && match_chain_pos == 0 && dist_next_match == 1 {
                pattern_path!('p2);
            }
            match_index -= dist_next_match;
        }
    }

    Match {
        len: longest,
        off: offset,
    }
}

fn find_longer_match(
    ctx: &mut HcCtx,
    buf: &[u8],
    ip: usize,
    ihigh_limit: usize,
    min_len: i32,
    nb_searches: i32,
    memo: &mut PatternMemo,
) -> Match {
    let md = insert_and_get_wider_match(
        ctx,
        buf,
        ip,
        ip,
        ihigh_limit,
        min_len,
        nb_searches,
        true,
        true,
        memo,
    );
    if md.len <= min_len {
        return Match::default();
    }

    md
}

#[inline]
const fn literals_price(litlen: i32) -> i32 {
    let mut price = litlen;
    if litlen >= RUN_MASK as i32 {
        price += 1 + (litlen - RUN_MASK as i32) / 255;
    }
    price
}

#[inline]
fn sequence_price(litlen: i32, mlen: i32) -> i32 {
    let mut price = 1 + 2;
    price += literals_price(litlen);
    if mlen >= (ML_MASK as i32 + MINMATCH) {
        price += 1 + (mlen - (ML_MASK as i32 + MINMATCH)) / 255;
    }
    price
}

fn encode_sequence(
    buf: &[u8],
    op: &mut Vec<u8>,
    ip: &mut usize,
    anchor: &mut usize,
    match_length: i32,
    offset: i32,
) {
    let length = *ip - *anchor;
    let token_pos = op.len();
    op.push(0);
    if length >= RUN_MASK as usize {
        let mut len = length - RUN_MASK as usize;
        op[token_pos] = (RUN_MASK << ML_BITS) as u8;
        while len >= 255 {
            op.push(255);
            len -= 255;
        }
        op.push(len as u8);
    } else {
        op[token_pos] = (length << ML_BITS) as u8;
    }

    op.extend_from_slice(&buf[*anchor..*anchor + length]);

    op.push((offset & 0xFF) as u8);
    op.push(((offset >> 8) & 0xFF) as u8);

    let mlen = (match_length - MINMATCH) as usize;
    if mlen >= ML_MASK as usize {
        op[token_pos] += ML_MASK as u8;
        let mut length = mlen - ML_MASK as usize;
        while length >= 510 {
            op.push(255);
            op.push(255);
            length -= 510;
        }
        if length >= 255 {
            length -= 255;
            op.push(255);
        }
        op.push(length as u8);
    } else {
        op[token_pos] += mlen as u8;
    }

    *ip += match_length as usize;
    *anchor = *ip;
}

#[derive(Clone, Copy, Default)]
struct Opt {
    price: i32,
    off: i32,
    mlen: i32,
    litlen: i32,
}

fn compress_optimal(buf: &[u8]) -> Vec<u8> {
    HC_CTX_POOL.with(|cell| {
        let mut ctx = cell.borrow_mut();
        ctx.prepare(buf.len());
        compress_optimal_with_ctx(buf, &mut ctx)
    })
}

fn compress_optimal_with_ctx(buf: &[u8], ctx: &mut HcCtx) -> Vec<u8> {
    let mut memo = PatternMemo::default();
    let src_size = buf.len();
    ctx.hi = ctx.base.saturating_add(src_size as u32);
    let mut op: Vec<u8> = Vec::with_capacity(src_size + src_size / 255 + 16);

    let mut opt = vec![Opt::default(); LZ4_OPT_NUM + TRAILING_LITERALS];

    let mut ip = 0usize;
    let mut anchor = 0usize;
    let iend = src_size;

    let mflimit = iend.wrapping_sub(MFLIMIT);
    let matchlimit = iend.wrapping_sub(LASTLITERALS);

    if src_size < LZ4_MINLENGTH {
        emit_last_literals(buf, &mut op, anchor, iend);
        return op;
    }

    while ip <= mflimit {
        let llen = (ip - anchor) as i32;
        let mut best_mlen;
        let mut best_off;
        let mut cur;
        let mut last_match_pos: i32;

        let first_match = find_longer_match(
            ctx,
            buf,
            ip,
            matchlimit,
            MINMATCH - 1,
            NB_SEARCHES,
            &mut memo,
        );
        if first_match.len == 0 {
            ip += 1;
            continue;
        }

        if first_match.len as usize > SUFFICIENT_LEN {
            let first_ml = first_match.len;
            encode_sequence(
                buf,
                &mut op,
                &mut ip,
                &mut anchor,
                first_ml,
                first_match.off,
            );
            continue;
        }

        for r_pos in 0..MINMATCH as usize {
            let cost = literals_price(llen + r_pos as i32);
            opt[r_pos].mlen = 1;
            opt[r_pos].off = 0;
            opt[r_pos].litlen = llen + r_pos as i32;
            opt[r_pos].price = cost;
        }

        {
            let match_ml = first_match.len;
            let offset = first_match.off;

            let mut cost = sequence_price(llen, MINMATCH);
            let mut next_bump = ML_MASK as i32 + MINMATCH;
            let mut mlen = MINMATCH;
            while mlen <= match_ml {
                let m = mlen as usize;
                opt[m].mlen = mlen;
                opt[m].off = offset;
                opt[m].litlen = llen;
                opt[m].price = cost;
                mlen += 1;
                if mlen == next_bump {
                    cost += 1;
                    next_bump += 255;
                }
            }
        }
        last_match_pos = first_match.len;
        for add_lit in 1..=TRAILING_LITERALS {
            let p = last_match_pos as usize + add_lit;
            opt[p].mlen = 1;
            opt[p].off = 0;
            opt[p].litlen = add_lit as i32;
            opt[p].price = opt[last_match_pos as usize].price + literals_price(add_lit as i32);
        }

        cur = 1i32;
        let mut goto_encode = false;
        while cur < last_match_pos {
            let cur_pos = ip + cur as usize;
            if cur_pos > mflimit {
                break;
            }

            let c = cur as usize;
            if opt[c + 1].price <= opt[c].price
                && opt[c + MINMATCH as usize].price < opt[c].price + 3
            {
                cur += 1;
                continue;
            }

            let new_match = find_longer_match(
                ctx,
                buf,
                cur_pos,
                matchlimit,
                MINMATCH - 1,
                NB_SEARCHES,
                &mut memo,
            );
            if new_match.len == 0 {
                cur += 1;
                continue;
            }

            if new_match.len as usize > SUFFICIENT_LEN
                || (new_match.len + cur) as usize >= LZ4_OPT_NUM
            {
                best_mlen = new_match.len;
                best_off = new_match.off;
                last_match_pos = cur + 1;

                encode_path(
                    buf,
                    &mut op,
                    &mut ip,
                    &mut anchor,
                    &mut opt,
                    cur,
                    last_match_pos,
                    best_mlen,
                    best_off,
                );
                goto_encode = true;
                break;
            }

            {
                let base_litlen = opt[c].litlen;
                for litlen in 1..MINMATCH {
                    let price = opt[c].price - literals_price(base_litlen)
                        + literals_price(base_litlen + litlen);
                    let pos = cur + litlen;
                    let p = pos as usize;
                    if price < opt[p].price {
                        opt[p].mlen = 1;
                        opt[p].off = 0;
                        opt[p].litlen = base_litlen + litlen;
                        opt[p].price = price;
                    }
                }
            }

            {
                let match_ml = new_match.len;
                let offset = new_match.off;

                let (ll, base_price) = if opt[c].mlen == 1 {
                    let ll = opt[c].litlen;
                    let prev = if cur > ll {
                        opt[(cur - ll) as usize].price
                    } else {
                        0
                    };
                    (ll, prev)
                } else {
                    (0, opt[c].price)
                };
                let mut price = base_price + sequence_price(ll, MINMATCH);
                let mut next_bump = ML_MASK as i32 + MINMATCH;
                let mut ml = MINMATCH;
                while ml <= match_ml {
                    let pos = cur + ml;
                    let p = pos as usize;
                    if pos > last_match_pos + TRAILING_LITERALS as i32 || price <= opt[p].price {
                        if ml == match_ml && last_match_pos < pos {
                            last_match_pos = pos;
                        }
                        opt[p].mlen = ml;
                        opt[p].off = offset;
                        opt[p].litlen = ll;
                        opt[p].price = price;
                    }
                    ml += 1;
                    if ml == next_bump {
                        price += 1;
                        next_bump += 255;
                    }
                }
            }

            for add_lit in 1..=TRAILING_LITERALS {
                let p = last_match_pos as usize + add_lit;
                opt[p].mlen = 1;
                opt[p].off = 0;
                opt[p].litlen = add_lit as i32;
                opt[p].price = opt[last_match_pos as usize].price + literals_price(add_lit as i32);
            }

            cur += 1;
        }

        if goto_encode {
            continue;
        }

        best_mlen = opt[last_match_pos as usize].mlen;
        best_off = opt[last_match_pos as usize].off;
        cur = last_match_pos - best_mlen;

        encode_path(
            buf,
            &mut op,
            &mut ip,
            &mut anchor,
            &mut opt,
            cur,
            last_match_pos,
            best_mlen,
            best_off,
        );
    }

    emit_last_literals(buf, &mut op, anchor, iend);
    op
}

#[allow(clippy::too_many_arguments)]
fn encode_path(
    buf: &[u8],
    op: &mut Vec<u8>,
    ip: &mut usize,
    anchor: &mut usize,
    opt: &mut [Opt],
    cur: i32,
    last_match_pos: i32,
    best_mlen: i32,
    best_off: i32,
) {
    {
        let mut candidate_pos = cur;
        let mut selected_matchlength = best_mlen;
        let mut selected_offset = best_off;
        loop {
            let cp = candidate_pos as usize;
            let next_matchlength = opt[cp].mlen;
            let next_offset = opt[cp].off;
            opt[cp].mlen = selected_matchlength;
            opt[cp].off = selected_offset;
            selected_matchlength = next_matchlength;
            selected_offset = next_offset;
            if next_matchlength > candidate_pos {
                break;
            }
            candidate_pos -= next_matchlength;
        }
    }

    let mut r_pos = 0i32;
    while r_pos < last_match_pos {
        let ml = opt[r_pos as usize].mlen;
        let offset = opt[r_pos as usize].off;
        if ml == 1 {
            *ip += 1;
            r_pos += 1;
            continue;
        }
        r_pos += ml;
        encode_sequence(buf, op, ip, anchor, ml, offset);
    }
}

fn emit_last_literals(buf: &[u8], op: &mut Vec<u8>, anchor: usize, iend: usize) {
    let last_run_size = iend - anchor;
    if last_run_size >= RUN_MASK as usize {
        let mut accumulator = last_run_size - RUN_MASK as usize;
        op.push((RUN_MASK << ML_BITS) as u8);
        while accumulator >= 255 {
            op.push(255);
            accumulator -= 255;
        }
        op.push(accumulator as u8);
    } else {
        op.push((last_run_size << ML_BITS) as u8);
    }
    op.extend_from_slice(&buf[anchor..anchor + last_run_size]);
}

pub fn compress_hc(src: &[u8]) -> Vec<u8> {
    compress_optimal(src)
}

#[cfg(test)]
mod hc_kernel_tests {
    use super::*;

    fn xorshift_fill(buf: &mut [u8], mut x: u64) {
        for b in buf.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = (x >> 32) as u8;
        }
    }

    /// The batch hasher must agree with `hash_ptr` at every position and
    /// alignment: `insert` mixes both paths, so any divergence changes match
    /// discovery and therefore compressed bytes.
    #[test]
    fn hash_batch_8_matches_hash_ptr() {
        let mut buf = vec![0u8; 4096];
        xorshift_fill(&mut buf, 0x9E3779B97F4A7C15);
        for pos0 in 0..buf.len() - 12 {
            let mut out = [0u32; 8];
            hash_batch_8(&buf, pos0, &mut out);
            for (k, &h) in out.iter().enumerate() {
                assert_eq!(h, hash_ptr(&buf, pos0 + k), "pos0={pos0} k={k}");
            }
        }
    }

    /// Digest of compress_hc over a deterministic mixed corpus. Run once on
    /// the unmodified build to capture the digest, then export
    /// ABGEN_LZ4_CORPUS_DIGEST=<hex> when running on a modified build to
    /// assert bit-identical output.
    #[test]
    fn compress_hc_corpus_digest() {
        let mut cases: Vec<Vec<u8>> = vec![Vec::new(), b"short".to_vec()];
        let mut rnd = vec![0u8; 1 << 20];
        xorshift_fill(&mut rnd, 0x0123456789ABCDEF);
        cases.push(rnd.clone());
        let mut rep = Vec::new();
        for _ in 0..64 {
            rep.extend_from_slice(&rnd[..8192]);
        }
        cases.push(rep);
        let mut mixed = vec![0u8; 1 << 20];
        for (i, b) in mixed.iter_mut().enumerate() {
            *b = ((i / 977) % 251) as u8;
        }
        cases.push(mixed);
        let mut pat = vec![0xABu8; 1 << 16];
        pat[12345] = 0x01;
        cases.push(pat);

        let mut h = crate::hashes::Sha256::new();
        for case in &cases {
            let comp = compress_hc(case);
            let back = decompress(&comp, case.len()).expect("round trip");
            assert_eq!(&back, case);
            h.update(&(comp.len() as u64).to_le_bytes());
            h.update(&comp);
        }
        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if let Ok(want) = std::env::var("ABGEN_LZ4_CORPUS_DIGEST") {
            assert_eq!(hex, want, "compress_hc output changed");
        } else {
            eprintln!("lz4 corpus digest: {hex}");
        }
    }

    fn reuse_cases() -> Vec<Vec<u8>> {
        let mut cases: Vec<Vec<u8>> = vec![Vec::new(), b"short".to_vec()];
        for period in [1usize, 2, 3, 4, 5, 6, 7, 8, 11, 16] {
            let mut v = vec![0u8; 96 * 1024];
            for (i, b) in v.iter_mut().enumerate() {
                *b = ((i % period) as u8)
                    .wrapping_mul(37)
                    .wrapping_add(period as u8);
            }
            v[1000] ^= 0x5A;
            v[65535] ^= 0xA5;
            cases.push(v);
        }
        let mut rnd = vec![0u8; 128 * 1024];
        xorshift_fill(&mut rnd, 0xDEADBEEFCAFEF00D);
        cases.push(rnd.clone());
        let mut half = rnd[..64 * 1024].to_vec();
        half.extend_from_slice(&rnd[..64 * 1024]);
        cases.push(half);
        cases
    }

    /// A reused (rebased) context must compress exactly like a fresh one:
    /// `prepare` promises stale table entries are unobservable.
    #[test]
    fn hc_ctx_reuse_matches_fresh() {
        let cases = reuse_cases();
        for pass in 0..2 {
            for (i, case) in cases.iter().enumerate() {
                let reused = compress_hc(case);
                let fresh = compress_optimal_with_ctx(case, &mut HcCtx::new());
                assert_eq!(reused, fresh, "pass={pass} case={i}");
                assert_eq!(decompress(&reused, case.len()).unwrap(), *case);
            }
        }
        let case = &cases[3];
        let fresh = compress_optimal_with_ctx(case, &mut HcCtx::new());
        let mut ctx = HcCtx::new();
        compress_optimal_with_ctx(case, &mut ctx);
        ctx.hi = u32::MAX - 100;
        ctx.prepare(case.len());
        assert_eq!(ctx.base, 64 * 1024, "expected the hard-reset path");
        assert_eq!(compress_optimal_with_ctx(case, &mut ctx), fresh);
    }

    /// Original byte-at-a-time decompressor, kept as the behavioral
    /// reference for the wildcopy version.
    fn decompress_ref(src: &[u8], dst_size: usize) -> Result<Vec<u8>, Lz4Error> {
        if dst_size > MAX_DECOMPRESS_BYTES {
            return Err(Lz4Error::Malformed(
                "decompressed size exceeds MAX_DECOMPRESS_BYTES",
            ));
        }
        let mut dst = vec![0u8; dst_size];
        let mut sp = 0usize;
        let mut dp = 0usize;
        let slen = src.len();

        if slen == 0 {
            if dst_size == 0 {
                return Ok(dst);
            }
            return Err(Lz4Error::Malformed("empty input but non-zero output"));
        }

        loop {
            if sp >= slen {
                return Err(Lz4Error::Malformed("truncated token"));
            }
            let token = src[sp] as u32;
            sp += 1;

            let mut lit_len = (token >> ML_BITS) as usize;
            if lit_len == RUN_MASK as usize {
                loop {
                    if sp >= slen {
                        return Err(Lz4Error::Malformed("truncated literal length"));
                    }
                    let b = src[sp];
                    sp += 1;
                    lit_len += b as usize;
                    if b != 255 {
                        break;
                    }
                }
            }

            if lit_len > 0 {
                if sp + lit_len > slen {
                    return Err(Lz4Error::Malformed("literal run exceeds input"));
                }
                if dp + lit_len > dst_size {
                    return Err(Lz4Error::Malformed("literal run exceeds output"));
                }
                dst[dp..dp + lit_len].copy_from_slice(&src[sp..sp + lit_len]);
                sp += lit_len;
                dp += lit_len;
            }

            if sp == slen {
                break;
            }

            if sp + 2 > slen {
                return Err(Lz4Error::Malformed("truncated offset"));
            }
            let offset = (src[sp] as usize) | ((src[sp + 1] as usize) << 8);
            sp += 2;
            if offset == 0 {
                return Err(Lz4Error::Malformed("zero offset"));
            }
            if offset > dp {
                return Err(Lz4Error::Malformed("offset before output start"));
            }

            let mut match_len = (token & ML_MASK) as usize;
            if match_len == ML_MASK as usize {
                loop {
                    if sp >= slen {
                        return Err(Lz4Error::Malformed("truncated match length"));
                    }
                    let b = src[sp];
                    sp += 1;
                    match_len += b as usize;
                    if b != 255 {
                        break;
                    }
                }
            }
            match_len += MINMATCH as usize;

            if dp + match_len > dst_size {
                return Err(Lz4Error::Malformed("match copy exceeds output"));
            }

            let mp = dp - offset;
            for k in 0..match_len {
                dst[dp + k] = dst[mp + k];
            }
            dp += match_len;
        }

        if dp != dst_size {
            return Err(Lz4Error::Malformed("decompressed size mismatch"));
        }
        Ok(dst)
    }

    /// The wildcopy decompressor must agree with the reference on valid
    /// streams (every overlap offset), on truncations, and on corruptions.
    #[test]
    fn decompress_matches_reference() {
        let mut streams: Vec<(Vec<u8>, usize)> = Vec::new();
        for case in reuse_cases() {
            let comp = compress_hc(&case);
            streams.push((comp, case.len()));
        }

        for (comp, dst_size) in &streams {
            let a = decompress(comp, *dst_size);
            let b = decompress_ref(comp, *dst_size);
            assert_eq!(a, b);
            for cut in [comp.len() / 3, comp.len() / 2, comp.len().saturating_sub(3)] {
                let a = decompress(&comp[..cut], *dst_size);
                let b = decompress_ref(&comp[..cut], *dst_size);
                assert_eq!(a, b, "cut={cut}");
            }
            let mut x = 0x123456789ABCDEFu64;
            for _ in 0..32 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let mut bad = comp.clone();
                let pos = (x as usize) % bad.len();
                bad[pos] ^= 1 << ((x >> 32) & 7);
                let a = decompress(&bad, *dst_size);
                let b = decompress_ref(&bad, *dst_size);
                assert_eq!(a, b, "flip at {pos}");
            }
        }
    }
}
