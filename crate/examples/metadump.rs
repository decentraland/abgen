//! Print the embedded metadata TextAsset JSON of each bundle, one line per
//! file: `<path>\t<m_Script JSON>` (or `<path>\t-` when the bundle carries no
//! metadata TextAsset). Used by scripts/verify-dep-casing.sh to assert the
//! per-platform CDN casing of `dependencies` entries.

use abgen::unity::bundle_file::{Bundle, FileContent};

fn main() {
    let mut exit = 0;
    for path in std::env::args().skip(1) {
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("{path}: read error: {e}");
                exit = 1;
                continue;
            }
        };
        let bundle = match Bundle::load_bytes(&data) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{path}: parse error: {e:#}");
                exit = 1;
                continue;
            }
        };
        let mut script: Option<String> = None;
        for f in &bundle.files {
            let FileContent::Serialized(sf) = &f.content else {
                continue;
            };
            for obj in &sf.objects {
                if obj.class_id != 49 {
                    continue;
                }
                let Ok(v) = sf.read_typetree(obj) else {
                    continue;
                };
                if v.get("m_Name").and_then(|x| x.as_str()) != Some("metadata") {
                    continue;
                }
                script = v
                    .get("m_Script")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string());
            }
        }
        println!("{path}\t{}", script.as_deref().unwrap_or("-"));
    }
    std::process::exit(exit);
}
