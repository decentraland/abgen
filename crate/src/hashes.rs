const fn crc32_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut n = 0usize;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
}

static CRC32_TABLE: [u32; 256] = crc32_table();

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = CRC32_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// The scalar SHA-256 kept for wasm32 (where binary size outweighs speed) and
/// as the test reference the sha2-backed path is checked against.
#[cfg(any(test, target_arch = "wasm32"))]
mod scalar {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    const H0: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    #[derive(Clone)]
    pub struct Sha256 {
        h: [u32; 8],
        buf: [u8; 64],
        buf_len: usize,
        total: u64,
    }

    impl Default for Sha256 {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Sha256 {
        pub const fn new() -> Self {
            Sha256 {
                h: H0,
                buf: [0; 64],
                buf_len: 0,
                total: 0,
            }
        }

        pub fn update(&mut self, mut data: &[u8]) {
            self.total = self.total.wrapping_add(data.len() as u64);
            if self.buf_len > 0 {
                let need = 64 - self.buf_len;
                let take = need.min(data.len());
                self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
                self.buf_len += take;
                data = &data[take..];
                if self.buf_len == 64 {
                    let block = self.buf;
                    self.compress(&block);
                    self.buf_len = 0;
                }
            }
            while data.len() >= 64 {
                let mut block = [0u8; 64];
                block.copy_from_slice(&data[..64]);
                self.compress(&block);
                data = &data[64..];
            }
            if !data.is_empty() {
                self.buf[..data.len()].copy_from_slice(data);
                self.buf_len = data.len();
            }
        }

        pub fn finalize(mut self) -> [u8; 32] {
            let bit_len = self.total.wrapping_mul(8);

            let mut pad = [0u8; 72];
            pad[0] = 0x80;
            let pad_len = if self.buf_len < 56 {
                56 - self.buf_len
            } else {
                120 - self.buf_len
            };
            self.update_internal(&pad[..pad_len]);
            self.update_internal(&bit_len.to_be_bytes());
            debug_assert_eq!(self.buf_len, 0);
            let mut out = [0u8; 32];
            for (i, word) in self.h.iter().enumerate() {
                out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
            }
            out
        }

        fn update_internal(&mut self, mut data: &[u8]) {
            if self.buf_len > 0 {
                let need = 64 - self.buf_len;
                let take = need.min(data.len());
                self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
                self.buf_len += take;
                data = &data[take..];
                if self.buf_len == 64 {
                    let block = self.buf;
                    self.compress(&block);
                    self.buf_len = 0;
                }
            }
            while data.len() >= 64 {
                let mut block = [0u8; 64];
                block.copy_from_slice(&data[..64]);
                self.compress(&block);
                data = &data[64..];
            }
            if !data.is_empty() {
                self.buf[..data.len()].copy_from_slice(data);
                self.buf_len = data.len();
            }
        }

        fn compress(&mut self, block: &[u8; 64]) {
            let mut w = [0u32; 64];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([
                    block[i * 4],
                    block[i * 4 + 1],
                    block[i * 4 + 2],
                    block[i * 4 + 3],
                ]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let mut v = self.h;
            for i in 0..64 {
                let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
                let ch = (v[4] & v[5]) ^ ((!v[4]) & v[6]);
                let t1 = v[7]
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
                let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
                let t2 = s0.wrapping_add(maj);
                v[7] = v[6];
                v[6] = v[5];
                v[5] = v[4];
                v[4] = v[3].wrapping_add(t1);
                v[3] = v[2];
                v[2] = v[1];
                v[1] = v[0];
                v[0] = t1.wrapping_add(t2);
            }
            for i in 0..8 {
                self.h[i] = self.h[i].wrapping_add(v[i]);
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use scalar::Sha256;

/// Streaming SHA-256 over the `sha2` crate, which runtime-dispatches to the
/// aarch64 SHA2 (and x86 SHA-NI) instructions — the pipeline hashes MB-scale
/// payloads per asset, so the hardware compress function matters. The state is
/// lazily materialized because `sha2`'s hasher has no const constructor and
/// `new()` must stay `const`.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Default)]
pub struct Sha256 {
    inner: Option<sha2::Sha256>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Sha256 {
    pub const fn new() -> Self {
        Sha256 { inner: None }
    }

    pub fn update(&mut self, data: &[u8]) {
        use sha2::Digest;
        self.inner
            .get_or_insert_with(sha2::Sha256::new)
            .update(data);
    }

    pub fn finalize(self) -> [u8; 32] {
        use sha2::Digest;
        let mut out = [0u8; 32];
        out.copy_from_slice(&self.inner.unwrap_or_default().finalize());
        out
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

pub fn sha256_hex(data: &[u8]) -> String {
    let d = sha256(data);
    let mut s = String::with_capacity(64);
    for b in d {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_values() {
        assert_eq!(crc32(b"Base Layer"), 0x2d18_2308);
        assert_eq!(crc32(b"Loop"), 0x016d_b2d0);
        assert_eq!(crc32(b"GravityWeight"), 0x7d7f_be84);
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn sha256_nist_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256_incremental_matches_oneshot() {
        let data: Vec<u8> = (0..200u32).map(|i| (i * 37 % 256) as u8).collect();
        let oneshot = sha256(&data);
        let mut h = Sha256::new();
        for chunk in data.chunks(7) {
            h.update(chunk);
        }
        assert_eq!(h.finalize(), oneshot);
    }

    #[test]
    fn sha256_block_boundary() {
        for n in [55usize, 56, 63, 64, 65, 119, 120, 128] {
            let data = vec![0xABu8; n];
            let mut h = Sha256::new();
            h.update(&data);
            let got = h.finalize();

            assert_eq!(got, sha256(&data));
        }
    }

    /// xorshift so the corpus is identical everywhere without a dependency.
    fn fill(buf: &mut [u8], mut state: u64) {
        for b in buf.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = (state >> 32) as u8;
        }
    }

    fn scalar_digest(data: &[u8]) -> [u8; 32] {
        let mut h = scalar::Sha256::new();
        h.update(data);
        h.finalize()
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn sha256_matches_scalar_reference() {
        let mut data = vec![0u8; 4096];
        fill(&mut data, 0x9E37_79B9_7F4A_7C15);
        for len in 0..=4096usize {
            assert_eq!(
                sha256(&data[..len]),
                scalar_digest(&data[..len]),
                "len {len}"
            );
        }

        let mut big = vec![0u8; 5 * 1024 * 1024];
        fill(&mut big, 0x2545_F491_4F6C_DD1D);
        assert_eq!(sha256(&big), scalar_digest(&big));

        let mut streamed = Sha256::new();
        let mut reference = scalar::Sha256::new();
        for (i, chunk) in big.chunks(4093).enumerate() {
            let take = &chunk[..chunk.len().min(1 + i % chunk.len())];
            streamed.update(take);
            reference.update(take);
        }
        assert_eq!(streamed.finalize(), reference.finalize());
    }
}
