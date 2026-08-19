use super::*;
use std::borrow::Cow;

pub const ALL_TYPES_TEMPLATE: &str = "all-types.windows.bundle";

pub const REQUIRED_TEMPLATES: [&str; 4] = [
    ALL_TYPES_TEMPLATE,
    "animated-types.windows.bundle",
    "emote-types.windows.bundle",
    "skinned-types.windows.bundle",
];

const EMBEDDED: [(&str, &[u8]); 4] = [
    (
        ALL_TYPES_TEMPLATE,
        include_bytes!("../../../template/all-types.windows.bundle"),
    ),
    (
        "animated-types.windows.bundle",
        include_bytes!("../../../template/animated-types.windows.bundle"),
    ),
    (
        "emote-types.windows.bundle",
        include_bytes!("../../../template/emote-types.windows.bundle"),
    ),
    (
        "skinned-types.windows.bundle",
        include_bytes!("../../../template/skinned-types.windows.bundle"),
    ),
];

pub(super) fn embedded_template(file: &str) -> Option<&'static [u8]> {
    EMBEDDED.iter().find(|(n, _)| *n == file).map(|(_, b)| *b)
}

fn override_root() -> Option<PathBuf> {
    std::env::var("ABGEN_ROOT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

pub fn template_source() -> String {
    match override_root() {
        Some(root) => format!("{} (ABGEN_ROOT)", root.join("template").display()),
        None => "compiled into this build".to_string(),
    }
}

pub(super) fn read_from_root(root: &std::path::Path, file: &str) -> Result<Vec<u8>> {
    let path = root.join("template").join(file);
    std::fs::read(&path).with_context(|| {
        format!(
            "ABGEN_ROOT is set to {} but the build template {file} could not be read from \
             {} — fix the path, or unset ABGEN_ROOT to use the copy compiled into this build",
            root.display(),
            path.display()
        )
    })
}

fn template_bytes(file: &str) -> Result<Cow<'static, [u8]>> {
    match override_root() {
        Some(root) => read_from_root(&root, file).map(Cow::Owned),
        None => embedded_template(file)
            .map(Cow::Borrowed)
            .ok_or_else(|| anyhow!("no build template named {file} is compiled into this build")),
    }
}

pub fn template_available() -> bool {
    template_bytes(ALL_TYPES_TEMPLATE).is_ok()
}

pub fn templates_missing() -> Vec<String> {
    REQUIRED_TEMPLATES
        .iter()
        .filter(|f| template_bytes(f).is_err())
        .map(|f| f.to_string())
        .collect()
}

pub fn templates_missing_in(dir: &std::path::Path) -> Vec<String> {
    REQUIRED_TEMPLATES
        .iter()
        .filter(|f| !dir.join(f).is_file())
        .map(|f| f.to_string())
        .collect()
}

pub fn require_templates() -> Result<()> {
    for f in REQUIRED_TEMPLATES {
        template_bytes(f)?;
    }
    Ok(())
}

pub fn template_identity() -> String {
    use std::sync::OnceLock;
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let mut buf: Vec<u8> = Vec::new();
            for f in REQUIRED_TEMPLATES {
                buf.extend_from_slice(f.as_bytes());
                match template_bytes(f) {
                    Ok(b) => buf.extend_from_slice(&b),
                    Err(_) => buf.extend_from_slice(b"<unavailable>"),
                }
            }
            crate::hashes::sha256_hex(&buf)
        })
        .clone()
}

fn harvest(
    out: &mut HashMap<String, (SerializedType, Value)>,
    file: &str,
    mapping: &[(&str, &str)],
) -> Result<()> {
    let bundle = read_template_bundle(file)
        .map_err(|e| anyhow!("aux build template {file} unavailable: {e}"))?;
    if let Some(sf) = bundle.serialized() {
        for obj in &sf.objects {
            for (src, key) in mapping {
                if obj.type_name == *src && !out.contains_key(*key) {
                    if let Ok(tree) = sf.read_typetree(obj) {
                        let st = sf.types[obj.type_id as usize].clone();
                        out.insert(key.to_string(), (st, tree));
                    }
                }
            }
        }
    }
    Ok(())
}

fn build_aux_types() -> Result<HashMap<String, (SerializedType, Value)>> {
    let mut out: HashMap<String, (SerializedType, Value)> = HashMap::new();
    harvest(
        &mut out,
        "animated-types.windows.bundle",
        &[
            ("Animation", "Animation"),
            ("AnimationClip", "AnimationClip"),
        ],
    )?;
    harvest(
        &mut out,
        "emote-types.windows.bundle",
        &[
            ("Animator", "Animator"),
            ("AnimatorController", "AnimatorController"),
            ("AnimationClip", "AnimationClip_mecanim"),
        ],
    )?;
    harvest(
        &mut out,
        "skinned-types.windows.bundle",
        &[("SkinnedMeshRenderer", "SkinnedMeshRenderer")],
    )?;
    Ok(out)
}

fn aux_types() -> Result<&'static HashMap<String, (SerializedType, Value)>> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<std::result::Result<HashMap<String, (SerializedType, Value)>, String>> =
        OnceLock::new();
    let entry = CACHE.get_or_init(|| build_aux_types().map_err(|e| format!("{e:#}")));
    entry.as_ref().map_err(|e| anyhow!("{e}"))
}

fn template_all_bytes() -> Result<Cow<'static, [u8]>> {
    template_bytes(ALL_TYPES_TEMPLATE)
}

fn read_template_bundle(file: &str) -> std::result::Result<Bundle, String> {
    let bytes = template_bytes(file).map_err(|e| format!("{e:#}"))?;
    Bundle::load_bytes(&bytes).map_err(|e| format!("{e:#}"))
}

pub(super) fn load_template() -> Result<(
    Bundle,
    &'static HashMap<String, SerializedType>,
    &'static HashMap<String, Value>,
)> {
    type Cached = (
        crate::unity::bundle_file::DecompressedBundle,
        std::sync::Mutex<Option<Bundle>>,
        HashMap<String, SerializedType>,
        HashMap<String, Value>,
    );
    static CACHE: std::sync::OnceLock<std::result::Result<Cached, String>> =
        std::sync::OnceLock::new();
    let entry = CACHE.get_or_init(|| {
        let load = || -> Result<Cached> {
            let mm = template_all_bytes()?;
            let decompressed = Bundle::decompress_bytes(&mm)?;
            let bundle = Bundle::from_decompressed(&decompressed)?;
            let mut proto: HashMap<String, SerializedType> = HashMap::new();
            let mut base: HashMap<String, Value> = HashMap::new();
            {
                let sf = bundle
                    .serialized()
                    .ok_or_else(|| anyhow!("template has no serialized file"))?;
                for obj in &sf.objects {
                    if !proto.contains_key(&obj.type_name) {
                        proto.insert(
                            obj.type_name.clone(),
                            sf.types[obj.type_id as usize].clone(),
                        );
                    }
                    if !base.contains_key(&obj.type_name) {
                        base.insert(obj.type_name.clone(), sf.read_typetree(obj)?);
                    }
                }
            }
            for (key, (st, tree)) in aux_types()?.iter() {
                proto.entry(key.clone()).or_insert_with(|| st.clone());
                base.entry(key.clone()).or_insert_with(|| tree.clone());
            }
            Ok((
                decompressed,
                std::sync::Mutex::new(Some(bundle)),
                proto,
                base,
            ))
        };
        load().map_err(|e| e.to_string())
    });
    match entry {
        Ok((decompressed, first_bundle, proto, base)) => {
            let bundle = match first_bundle.lock().unwrap().take() {
                Some(b) => b,
                None => Bundle::from_decompressed(decompressed)?,
            };
            Ok((bundle, proto, base))
        }
        Err(e) => Err(anyhow!("{e}")),
    }
}

pub(super) fn cab_node_name(bundle: &Bundle) -> Result<String> {
    bundle
        .files
        .iter()
        .find(|e| !e.name.to_lowercase().ends_with(".ress"))
        .map(|e| e.name.clone())
        .ok_or_else(|| anyhow!("no SerializedFile node found in bundle container"))
}
