use anyhow::{Context, Result};
use memmap2::Mmap;
use std::fs::File;
use std::path::{Path, PathBuf};

pub fn mmap_file(path: &Path) -> Result<Mmap> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    unsafe { Mmap::map(&f) }.with_context(|| format!("mmap {}", path.display()))
}

pub const ABGEN_CONTENT_ROOT_ENV: &str = "ABGEN_CONTENT_ROOT";
pub const DEFAULT_CONTENT_ROOT: &str = "./content";

#[derive(Clone)]
pub struct LocalContentStore {
    root: PathBuf,
}

impl LocalContentStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Shards by the first two SHA-1 bytes of the cid; the filename is the cid itself,
    /// collapsed by `fs_safe_component` when it cannot fit in NAME_MAX. Content ids are
    /// unbounded input (the sdk-commands preview server derives them from absolute source
    /// paths, and content-versioned ids append an mtime + machine-id tail), so the store
    /// must stay valid for any id length. Reads and writes both resolve through here,
    /// which keeps the collapse symmetric without a mapping file, and a collapsed name
    /// can never collide with a verbatim one (verbatim names are short by definition).
    fn path_for(&self, cid: &str) -> PathBuf {
        use sha1::{Digest, Sha1};
        let digest = Sha1::digest(cid.as_bytes());
        let mut prefix = String::with_capacity(4);
        for b in &digest[..2] {
            prefix.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            prefix.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
        }
        self.root
            .join(prefix)
            .join(&*crate::naming::fs_safe_component(cid))
    }

    pub fn fetch(&self, cid: &str) -> Result<Vec<u8>> {
        let path = self.path_for(cid);
        let mm = mmap_file(&path)
            .with_context(|| format!("local content store: {} (CID {cid})", path.display(),))?;
        Ok(mm.to_vec())
    }

    pub fn fetch_mmap(&self, cid: &str) -> Result<Mmap> {
        let path = self.path_for(cid);
        mmap_file(&path)
            .with_context(|| format!("local content store: {} (CID {cid})", path.display(),))
    }

    pub fn exists(&self, cid: &str) -> bool {
        std::fs::metadata(self.path_for(cid))
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    }

    pub fn path_of(&self, cid: &str) -> PathBuf {
        self.path_for(cid)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn write(&self, cid: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path_for(cid);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create shard dir {}", dir.display()))?;
        }
        let tmp = crate::tmppath::tmp_sibling(&path);
        std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_for_uses_sha1_first_four_hex() {
        let s = LocalContentStore::new("/dev/null/x");
        let p = s.path_for("bafkreibxefote3jeusciwqxxrvwu5b4qi7uzg6lf3avexadfg7xkkz5gge");
        let prefix = p.parent().unwrap().file_name().unwrap().to_string_lossy();
        assert_eq!(prefix, "91f7");
    }

    #[test]
    fn fetch_missing_returns_clear_error() {
        let s = LocalContentStore::new("/nonexistent/abgen-local-store");
        let err = s.fetch("bafkrei000000").unwrap_err().to_string();
        assert!(err.contains("local content store"), "{err}");
    }

    #[test]
    fn oversized_cid_is_collapsed_and_roundtrips() {
        let tmp = std::env::temp_dir().join(format!("abgen_store_guard_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let s = LocalContentStore::new(&tmp);

        // Longer than any NAME_MAX: must be stored under a fixed-length collapsed name.
        let cid = format!("b64-{}", "Q".repeat(260));
        s.write(&cid, b"payload").unwrap();
        assert!(s.exists(&cid));
        assert_eq!(s.fetch(&cid).unwrap(), b"payload");

        let name = s.path_of(&cid);
        let name = name.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("xn-"), "collapsed name, got {name}");
        assert!(
            name.len() <= 46,
            "fixed-length name, got {} bytes",
            name.len()
        );

        // Distinct oversized cids must not share an entry.
        let other = format!("b64-{}", "R".repeat(260));
        assert_ne!(s.path_of(&cid), s.path_of(&other));

        // Names within the limit keep production behavior: stored verbatim.
        let ok_cid = format!("b64-{}", "Q".repeat(196));
        s.write(&ok_cid, b"x").unwrap();
        assert!(s.exists(&ok_cid));
        assert_eq!(
            s.path_of(&ok_cid).file_name().unwrap().to_string_lossy(),
            ok_cid
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
