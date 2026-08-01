//! The export request blob: one length-prefixed byte buffer, host-agnostic.
//!
//! Layout (all integers little-endian u32):
//!
//! ```text
//! u32 file_count
//!   repeated file_count times:
//!     u32 name_len, name_len bytes (utf-8 path)
//!     u32 data_len, data_len bytes
//! u32 len, len bytes  platform      ("windows" | "mac" | "linux" | "webgl")
//! u32 len, len bytes  entity_type   ("" = detect from the files)
//! u8  magenta_missing
//! u8  lod
//! u8  mode          (0 convert, 1 scan, 2 convert_only, 3 lod_only)
//! u8  crop
//! u32 tri_cap       (0 = uncapped)
//! u32 len, len bytes  entity_hash   ("" = derive; names the LOD, mode 3)
//! u32 len, len bytes  only_glb      (mode 2: which file to convert)
//! u32 entry_count                   (optional content table; 0 = derive)
//!   repeated entry_count times:
//!     u32 len, len bytes  file name
//!     u32 len, len bytes  content hash
//! ```
//!
//! Everything from `tri_cap` on is optional: a short buffer stops the parse
//! and the rest take defaults, so older hosts keep working.

/// A parsed export request.
pub struct Input {
    pub files: Vec<(String, Vec<u8>)>,
    pub platform: String,
    pub entity_type: String,
    pub magenta: bool,
    pub lod: bool,
    pub mode: u8,
    pub crop: bool,
    pub tri_cap: u32,
    pub entity_hash: Option<String>,
    pub only_glb: Option<String>,
    pub content_table: Option<Vec<(String, String)>>,
}

fn read_u32(buf: &[u8], off: &mut usize) -> Option<u32> {
    let b = buf.get(*off..off.checked_add(4)?)?;
    *off += 4;
    Some(u32::from_le_bytes(b.try_into().ok()?))
}

fn read_chunk<'a>(buf: &'a [u8], off: &mut usize) -> Option<&'a [u8]> {
    let len = read_u32(buf, off)? as usize;
    let b = buf.get(*off..off.checked_add(len)?)?;
    *off += len;
    Some(b)
}

fn read_str(buf: &[u8], off: &mut usize) -> Option<String> {
    String::from_utf8(read_chunk(buf, off)?.to_vec()).ok()
}

/// `None` means malformed — never a half-read request.
pub fn parse_input(buf: &[u8]) -> Option<Input> {
    let mut off = 0usize;
    let n = read_u32(buf, &mut off)? as usize;
    let mut files = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let name = read_str(buf, &mut off)?;
        let data = read_chunk(buf, &mut off)?.to_vec();
        files.push((name, data));
    }
    let platform = read_str(buf, &mut off)?;
    let entity_type = read_str(buf, &mut off)?;
    let magenta = *buf.get(off)? != 0;
    let lod = buf.get(off + 1).copied().unwrap_or(0) != 0;
    let mode = buf.get(off + 2).copied().unwrap_or(0);
    let crop = buf.get(off + 3).copied().unwrap_or(0) != 0;
    off = (off + 4).min(buf.len());
    let tri_cap = read_u32(buf, &mut off).unwrap_or(0);
    let entity_hash = read_str(buf, &mut off).filter(|s| !s.is_empty());
    let only_glb = read_str(buf, &mut off).filter(|s| !s.is_empty());
    let content_table = read_u32(buf, &mut off)
        .and_then(|n| {
            let mut t = Vec::with_capacity((n as usize).min(4096));
            for _ in 0..n {
                t.push((read_str(buf, &mut off)?, read_str(buf, &mut off)?));
            }
            Some(t)
        })
        .filter(|t| !t.is_empty());
    Some(Input {
        files,
        platform,
        entity_type,
        magenta,
        lod,
        mode,
        crop,
        tri_cap,
        entity_hash,
        only_glb,
        content_table,
    })
}

/// Builds a request blob, so embedders need not hand-roll the layout.
pub struct InputBuilder {
    files: Vec<(String, Vec<u8>)>,
    platform: String,
    entity_type: String,
    magenta: bool,
    lod: bool,
    mode: u8,
    crop: bool,
    tri_cap: u32,
    entity_hash: String,
    only_glb: String,
    content_table: Vec<(String, String)>,
}

impl Default for InputBuilder {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            platform: "windows".to_string(),
            entity_type: String::new(),
            magenta: false,
            lod: false,
            mode: 0,
            crop: false,
            tri_cap: 0,
            entity_hash: String::new(),
            only_glb: String::new(),
            content_table: Vec::new(),
        }
    }
}

impl InputBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn file(mut self, name: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        self.files.push((name.into(), data.into()));
        self
    }

    pub fn platform(mut self, p: impl Into<String>) -> Self {
        self.platform = p.into();
        self
    }

    pub fn entity_type(mut self, t: impl Into<String>) -> Self {
        self.entity_type = t.into();
        self
    }

    pub fn mode(mut self, mode: u8) -> Self {
        self.mode = mode;
        self
    }

    pub fn magenta(mut self, on: bool) -> Self {
        self.magenta = on;
        self
    }

    pub fn lod(mut self, on: bool) -> Self {
        self.lod = on;
        self
    }

    pub fn crop(mut self, on: bool) -> Self {
        self.crop = on;
        self
    }

    pub fn tri_cap(mut self, cap: u32) -> Self {
        self.tri_cap = cap;
        self
    }

    pub fn entity_hash(mut self, h: impl Into<String>) -> Self {
        self.entity_hash = h.into();
        self
    }

    pub fn only_glb(mut self, name: impl Into<String>) -> Self {
        self.only_glb = name.into();
        self
    }

    pub fn content_entry(mut self, name: impl Into<String>, hash: impl Into<String>) -> Self {
        self.content_table.push((name.into(), hash.into()));
        self
    }

    pub fn build(self) -> Vec<u8> {
        fn put(out: &mut Vec<u8>, bytes: &[u8]) {
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&(self.files.len() as u32).to_le_bytes());
        for (name, data) in &self.files {
            put(&mut out, name.as_bytes());
            put(&mut out, data);
        }
        put(&mut out, self.platform.as_bytes());
        put(&mut out, self.entity_type.as_bytes());
        out.push(self.magenta as u8);
        out.push(self.lod as u8);
        out.push(self.mode);
        out.push(self.crop as u8);
        out.extend_from_slice(&self.tri_cap.to_le_bytes());
        put(&mut out, self.entity_hash.as_bytes());
        put(&mut out, self.only_glb.as_bytes());
        out.extend_from_slice(&(self.content_table.len() as u32).to_le_bytes());
        for (name, hash) in &self.content_table {
            put(&mut out, name.as_bytes());
            put(&mut out, hash.as_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_a_full_request() {
        let blob = InputBuilder::new()
            .file("model.glb", vec![1u8, 2, 3])
            .file("tex.png", vec![4u8, 5])
            .platform("mac")
            .entity_type("wearable")
            .magenta(true)
            .lod(true)
            .mode(2)
            .crop(true)
            .tri_cap(4096)
            .entity_hash("bafyhash")
            .only_glb("model.glb")
            .content_entry("model.glb", "hash1")
            .build();

        let got = parse_input(&blob).expect("parses");
        assert_eq!(got.files.len(), 2);
        assert_eq!(got.files[0].0, "model.glb");
        assert_eq!(got.files[0].1, vec![1, 2, 3]);
        assert_eq!(got.platform, "mac");
        assert_eq!(got.entity_type, "wearable");
        assert!(got.magenta);
        assert!(got.lod);
        assert_eq!(got.mode, 2);
        assert!(got.crop);
        assert_eq!(got.tri_cap, 4096);
        assert_eq!(got.entity_hash.as_deref(), Some("bafyhash"));
        assert_eq!(got.only_glb.as_deref(), Some("model.glb"));
        assert_eq!(got.content_table.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn optional_tail_defaults_when_truncated() {
        let mut blob = InputBuilder::new().file("a.glb", vec![9u8]).build();
        blob.truncate(blob.len() - 12);
        let got = parse_input(&blob).expect("parses without the optional tail");
        assert_eq!(got.tri_cap, 0);
        assert!(got.entity_hash.is_none());
        assert!(got.content_table.is_none());
    }

    #[test]
    fn rejects_a_truncated_file_table() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&4u32.to_le_bytes());
        blob.extend_from_slice(&1u32.to_le_bytes());
        blob.push(b'a');
        assert!(parse_input(&blob).is_none());
    }

    /// A four-byte count must not become a reservation: allocation failure
    /// aborts, so this is the one malformed input that could crash the host.
    #[test]
    fn a_huge_content_table_count_does_not_reserve() {
        let mut blob = InputBuilder::new().file("a.glb", vec![1u8]).build();
        let n = blob.len();
        blob[n - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
        let got = parse_input(&blob).expect("parses");
        assert!(got.content_table.is_none());
    }

    #[test]
    fn rejects_an_empty_buffer() {
        assert!(parse_input(&[]).is_none());
    }

    #[test]
    fn rejects_a_length_that_overflows_the_buffer() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&1u32.to_le_bytes());
        blob.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_input(&blob).is_none());
    }
}
