use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::model::LodModel;

pub const CLASS_SURVIVAL_MIN_TRIS: usize = 200;
pub const CLASS_RESCUE_ERROR_LIMIT: f64 = 0.001;
// gltfpack's default -se 0.01 stalls far above deep caps, while -sa welds the
// mesh to ~0.5 verts/tri and destroys UVs (prod ships 1.5-2.4 verts/tri at the
// cap); 0.25 keeps the seam-preserving simplifier and sits under the measured
// -se ~0.5 cliff where meshes collapse far past the ratio target
pub const DEEP_ERROR_LIMIT: f64 = 0.25;

pub const GLTFPACK_NIX_RECIPE: &str =
    "nix-shell -p meshoptimizer --run 'gltfpack -i <in.glb> -o <out.glb> -si 0.1 -noq'";

pub const SUBPROC_TIMEOUT_ENV: &str = "ABGEN_LOD_SUBPROC_TIMEOUT_S";

pub const SIMPLIFIER_ENV: &str = "ABGEN_SIMPLIFIER";

pub const DEFAULT_SIMPLIFIER: SimplifierBackend = SimplifierBackend::Meshopt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimplifierBackend {
    Meshopt,
    Gltfpack,
}

impl SimplifierBackend {
    pub fn name(self) -> &'static str {
        match self {
            SimplifierBackend::Meshopt => "meshopt",
            SimplifierBackend::Gltfpack => "gltfpack",
        }
    }

    pub fn parse(s: &str) -> Result<SimplifierBackend> {
        match s.trim().to_ascii_lowercase().as_str() {
            "meshopt" => Ok(SimplifierBackend::Meshopt),
            "gltfpack" => Ok(SimplifierBackend::Gltfpack),
            other => bail!("unknown simplifier {other:?} (want meshopt|gltfpack)"),
        }
    }

    pub fn from_env() -> SimplifierBackend {
        match std::env::var(SIMPLIFIER_ENV) {
            Ok(v) if !v.trim().is_empty() => match SimplifierBackend::parse(&v) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!(
                        "WARNING: {SIMPLIFIER_ENV}={v:?} ignored ({e:#}); using {}",
                        DEFAULT_SIMPLIFIER.name()
                    );
                    DEFAULT_SIMPLIFIER
                }
            },
            _ => DEFAULT_SIMPLIFIER,
        }
    }
}

pub fn subproc_deadline() -> Option<Duration> {
    let secs: u64 = std::env::var(SUBPROC_TIMEOUT_ENV)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    }
}

pub fn run_with_deadline(
    mut cmd: Command,
    deadline: Option<Duration>,
    label: &str,
) -> Result<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().with_context(|| format!("spawn {label}"))?;
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_h = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_end(&mut b);
        }
        b
    });
    let err_h = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut b);
        }
        b
    });
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().with_context(|| format!("wait {label}"))? {
            break st;
        }
        if let Some(d) = deadline {
            if started.elapsed() > d {
                #[cfg(unix)]
                unsafe {
                    libc::killpg(child.id() as i32, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_h.join();
                let _ = err_h.join();
                bail!(
                    "{label} exceeded the {}s subprocess deadline ({SUBPROC_TIMEOUT_ENV}); killed",
                    d.as_secs()
                );
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let stdout = out_h.join().unwrap_or_default();
    let stderr = err_h.join().unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

pub use super::simplify_report::SimplifyReport;

pub fn rescale_ratio(prev: f64, actual: u64, target: u64) -> f64 {
    if actual == 0 {
        return prev;
    }
    (prev * (target as f64 / actual as f64) * 0.9).clamp(1e-3, 1.0)
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn resolve_from(
    flag: Option<&Path>,
    env: Option<&std::ffi::OsStr>,
    path_var: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    if let Some(f) = flag {
        return Ok(f.to_path_buf());
    }
    if let Some(e) = env {
        if !e.is_empty() {
            return Ok(PathBuf::from(e));
        }
    }
    if let Some(pv) = path_var {
        for dir in std::env::split_paths(pv) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let cand = dir.join("gltfpack");
            if is_executable(&cand) {
                return Ok(cand);
            }
        }
    }
    bail!(
        "gltfpack not found (checked --gltfpack, ABGEN_GLTFPACK, PATH); install \
         meshoptimizer's gltfpack (1.1), e.g. {GLTFPACK_NIX_RECIPE}"
    )
}

pub fn resolve_gltfpack(flag: Option<&Path>) -> Result<PathBuf> {
    let env = std::env::var_os("ABGEN_GLTFPACK");
    let path_var = std::env::var_os("PATH");
    resolve_from(flag, env.as_deref(), path_var.as_deref())
}

// no -sp: permissive simplification prunes whole small components, deleting
// salient geometry (tree canopies, alpha-class foliage) the production
// reference preserves at the same triangle budget
fn simplify_args(ratio: f64, aggressive: bool, error_limit: Option<f64>) -> Vec<String> {
    let mut args = vec!["-si".to_string(), format!("{ratio}")];
    if let Some(e) = error_limit {
        args.push("-se".to_string());
        args.push(format!("{e}"));
    }
    if aggressive {
        args.push("-sa".to_string());
    }
    args.push("-noq".to_string());
    args
}

fn run_gltfpack(
    gltfpack: &Path,
    input: &Path,
    output: &Path,
    ratio: f64,
    aggressive: bool,
    error_limit: Option<f64>,
) -> Result<()> {
    let mut cmd = Command::new(gltfpack);
    cmd.arg("-i").arg(input).arg("-o").arg(output);
    cmd.args(simplify_args(ratio, aggressive, error_limit));
    let out = run_with_deadline(
        cmd,
        subproc_deadline(),
        &format!("gltfpack -si {ratio} ({})", gltfpack.display()),
    )
    .with_context(|| {
        format!(
            "run {} (if missing: {GLTFPACK_NIX_RECIPE})",
            gltfpack.display()
        )
    })?;
    if !out.status.success() {
        bail!(
            "gltfpack -si {ratio}{} failed ({}): {}{}",
            if aggressive { " -sa" } else { "" },
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

fn glb_tris(path: &Path) -> Result<usize> {
    Ok(glb_model(path)?.total_tris())
}

fn glb_model(path: &Path) -> Result<LodModel> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    super::model::from_glb_bytes(&bytes, "simplify-check")
        .with_context(|| format!("reparse {}", path.display()))
}

fn tris_by_material(model: &LodModel) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for prim in &model.primitives {
        if let Some(mat) = model.materials.get(prim.material) {
            *counts.entry(mat.name.clone()).or_insert(0) += prim.indices.len() / 3;
        }
    }
    counts
}

fn class_submodel(model: &LodModel, mat_idx: usize) -> LodModel {
    let mat = &model.materials[mat_idx];
    let mut sub = LodModel {
        root_name: format!("{}-rescue", model.root_name),
        materials: vec![super::model::LodMaterial {
            image: None,
            ..mat.clone()
        }],
        ..Default::default()
    };
    if let Some(img) = mat.image {
        sub.images.push(model.images[img].clone());
        sub.materials[0].image = Some(0);
    }
    for prim in &model.primitives {
        if prim.material == mat_idx {
            let mut p = prim.clone();
            p.material = 0;
            sub.primitives.push(p);
        }
    }
    sub
}

fn merge_class(out_model: &mut LodModel, source: &LodModel, mat_idx: usize, geometry: &LodModel) {
    let mat = &source.materials[mat_idx];
    let merged_mat = match out_model.materials.iter().position(|m| m.name == mat.name) {
        Some(i) => i,
        None => {
            let mut m = super::model::LodMaterial {
                image: None,
                ..mat.clone()
            };
            if let Some(img) = mat.image {
                out_model.images.push(source.images[img].clone());
                m.image = Some(out_model.images.len() - 1);
            }
            out_model.materials.push(m);
            out_model.materials.len() - 1
        }
    };
    for prim in &geometry.primitives {
        let mut p = prim.clone();
        p.material = merged_mat;
        out_model.primitives.push(p);
    }
}

// gltfpack's per-mesh ratio target can delete an entire material class made of
// small scattered pieces (foliage cards); production always keeps every class,
// so re-run vanished classes alone with a tightened error limit and merge back
fn rescue_lost_classes(
    input: &Path,
    output: &Path,
    ratio: f64,
    gltfpack: &Path,
    report: &mut SimplifyReport,
) -> Result<()> {
    let in_model = glb_model(input)?;
    let in_tris = tris_by_material(&in_model);
    let mut out_model = glb_model(output)?;
    let out_tris = tris_by_material(&out_model);
    let mut changed = false;
    for mat_idx in 0..in_model.materials.len() {
        let name = in_model.materials[mat_idx].name.clone();
        let before = in_tris.get(&name).copied().unwrap_or(0);
        if before < CLASS_SURVIVAL_MIN_TRIS || out_tris.get(&name).copied().unwrap_or(0) > 0 {
            continue;
        }
        let sub = class_submodel(&in_model, mat_idx);
        let sub_in = output.with_extension("rescue-in.glb");
        let sub_out = output.with_extension("rescue-out.glb");
        std::fs::write(&sub_in, super::emit::emit_glb(&sub)?)
            .with_context(|| format!("write {}", sub_in.display()))?;
        run_gltfpack(
            gltfpack,
            &sub_in,
            &sub_out,
            ratio,
            false,
            Some(CLASS_RESCUE_ERROR_LIMIT),
        )?;
        let rescued = glb_model(&sub_out)?;
        let _ = std::fs::remove_file(&sub_in);
        let _ = std::fs::remove_file(&sub_out);
        if rescued.total_tris() > 0 {
            eprintln!(
                "simplify: class {name:?} vanished at ratio {ratio} ({before} source tris); \
                 rescued alone with -se {CLASS_RESCUE_ERROR_LIMIT} ({} tris)",
                rescued.total_tris()
            );
            merge_class(&mut out_model, &in_model, mat_idx, &rescued);
        } else {
            eprintln!(
                "simplify: class {name:?} vanished at ratio {ratio} ({before} source tris) and \
                 the -se {CLASS_RESCUE_ERROR_LIMIT} retry also emptied it; merging it verbatim"
            );
            merge_class(
                &mut out_model,
                &in_model,
                mat_idx,
                &class_submodel(&in_model, mat_idx),
            );
        }
        report.rescued_classes.push(name);
        changed = true;
    }
    if changed {
        std::fs::write(output, super::emit::emit_glb(&out_model)?)
            .with_context(|| format!("write {}", output.display()))?;
        report.tris_after = out_model.total_tris();
    }
    Ok(())
}

fn copy_through(input: &Path, output: &Path) -> Result<()> {
    if input != output {
        std::fs::copy(input, output)
            .with_context(|| format!("copy {} -> {}", input.display(), output.display()))?;
    }
    Ok(())
}

pub fn passthrough(input: &Path, output: &Path) -> Result<SimplifyReport> {
    let tris = glb_tris(input)?;
    copy_through(input, output)?;
    Ok(SimplifyReport {
        tris_before: tris,
        tris_after: tris,
        passthrough: true,
        ..Default::default()
    })
}

pub fn copy_unsimplified(input: &Path, output: &Path) -> Result<SimplifyReport> {
    let mut report = passthrough(input, output)?;
    report.unsimplified = true;
    eprintln!(
        "WARNING: --allow-unsimplified: {} copied through VERBATIM ({} tris, no decimation); \
         this is a completeness escape hatch, not a production mode",
        input.display(),
        report.tris_before
    );
    Ok(report)
}

pub fn simplify(
    input: &Path,
    output: &Path,
    ratio: f64,
    tri_cap: Option<u64>,
    gltfpack: &Path,
) -> Result<SimplifyReport> {
    let tris_before = glb_tris(input)?;
    let under_cap = |tris: usize| tri_cap.is_none_or(|c| tris as u64 <= c);
    if ratio >= 1.0 && under_cap(tris_before) {
        copy_through(input, output)?;
        return Ok(SimplifyReport {
            tris_before,
            tris_after: tris_before,
            passthrough: true,
            ..Default::default()
        });
    }
    let mut report = SimplifyReport {
        tris_before,
        ..Default::default()
    };
    let mut current = ratio.clamp(1e-3, 1.0);
    let mut tris_after = tris_before;
    let mut mode = (false, None);
    for attempt in 0..5 {
        let (aggressive, error_limit) = match attempt {
            0 | 1 => (false, None),
            2 | 3 => (false, Some(DEEP_ERROR_LIMIT)),
            _ => (true, None),
        };
        run_gltfpack(gltfpack, input, output, current, aggressive, error_limit)?;
        report.ratios_run.push(current);
        report.aggressive_final = aggressive;
        tris_after = glb_tris(output)?;
        if under_cap(tris_after) {
            mode = (aggressive, error_limit);
            break;
        }
        if aggressive {
            bail!(
                "tri cap {} not reached after {} gltfpack attempts (final {} tris, ratios {:?})",
                tri_cap.unwrap_or(0),
                report.ratios_run.len(),
                tris_after,
                report.ratios_run
            );
        }
        current = if attempt == 0 || attempt == 2 {
            rescale_ratio(current, tris_after as u64, tri_cap.unwrap_or(1))
        } else {
            rescale_ratio(1.0, tris_before as u64, tri_cap.unwrap_or(1))
        };
    }
    let mut error_relaxed = false;
    if let Some(cap) = tri_cap {
        if tris_after as u64 > cap {
            error_relaxed = true;
            let ratio_sa = rescale_ratio(1.0, tris_before as u64, cap.max(1));
            let (mut lo, mut hi) = (0.01f64, 1.0f64);
            let mut best: Option<(usize, Vec<u8>)> = None;
            for _ in 0..6 {
                let se = (lo + hi) / 2.0;
                run_gltfpack(gltfpack, input, output, ratio_sa, true, Some(se))?;
                report.ratios_run.push(ratio_sa);
                report.aggressive_final = true;
                let t = glb_tris(output)?;
                if t as u64 <= cap {
                    hi = se;
                    if best.as_ref().is_none_or(|(bt, _)| t > *bt) {
                        best = Some((t, std::fs::read(output)?));
                    }
                } else {
                    lo = se;
                }
            }
            match best {
                Some((t, bytes)) => {
                    std::fs::write(output, &bytes)?;
                    tris_after = t;
                }
                None => bail!(
                    "tri cap {cap} not reached after {} gltfpack attempts (final {} tris, ratios {:?})",
                    report.ratios_run.len(),
                    tris_after,
                    report.ratios_run
                ),
            }
        }
    }
    if let Some(cap) = tri_cap.filter(|_| !error_relaxed) {
        let floor = (cap as f64 * 0.8) as usize;
        if tris_after > 0
            && (tris_before as u64) > cap
            && (tris_after as u64) <= cap
            && tris_after < floor
        {
            let mut lo_ratio = report.ratios_run.last().copied().unwrap_or(current);
            let mut hi_ratio: Option<f64> = None;
            let mut best = std::fs::read(output)?;
            for _ in 0..6 {
                if tris_after >= floor {
                    break;
                }
                let cand = match hi_ratio {
                    None => (cap as f64 * 0.9 / tris_before as f64)
                        .max(lo_ratio * 2.0)
                        .clamp(1e-3, 1.0),
                    Some(h) => (lo_ratio + h) / 2.0,
                };
                if (cand - lo_ratio).abs() < 1e-6 {
                    break;
                }
                run_gltfpack(gltfpack, input, output, cand, mode.0, mode.1)?;
                let t = glb_tris(output)?;
                if t as u64 > cap {
                    hi_ratio = Some(cand);
                } else {
                    lo_ratio = cand;
                    if t > tris_after {
                        tris_after = t;
                        best = std::fs::read(output)?;
                        report.ratios_run.push(cand);
                    }
                }
            }
            std::fs::write(output, &best)?;
        }
    }
    report.tris_after = tris_after;
    let ratio_run = report.ratios_run.last().copied().unwrap_or(current);
    rescue_lost_classes(input, output, ratio_run, gltfpack, &mut report)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lodgen::emit::emit_glb;
    use crate::lodgen::model::{AlphaClass, LodMaterial, LodModel, LodPrimitive};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "abgen-lod-simplify-test-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn grid_primitive(n: u32) -> LodPrimitive {
        let mut positions = Vec::new();
        let mut normals = Vec::new();
        let mut uvs = Vec::new();
        for j in 0..=n {
            for i in 0..=n {
                let x = i as f32 / n as f32;
                let z = j as f32 / n as f32;
                let y = 0.05
                    * ((x * 12.0).sin() + (z * 12.0).cos())
                    * (1.0 + 0.3 * ((x * 5.0 + z * 7.0).sin()));
                positions.push([x * 10.0, y, z * 10.0]);
                normals.push([0.0, 1.0, 0.0]);
                uvs.push([x, z]);
            }
        }
        let mut indices = Vec::new();
        for j in 0..n {
            for i in 0..n {
                let a = j * (n + 1) + i;
                let b = a + 1;
                let c = a + n + 1;
                let d = c + 1;
                indices.extend_from_slice(&[a, c, b, b, c, d]);
            }
        }
        LodPrimitive {
            positions,
            normals,
            uvs,
            indices,
            material: 0,
            ..Default::default()
        }
    }

    fn material(name: &str, class: AlphaClass) -> LodMaterial {
        LodMaterial {
            name: name.to_string(),
            class,
            base_color: [1.0, 1.0, 1.0, 1.0],
            cutoff: 0.5,
            image: None,
            double_sided: false,
        }
    }

    fn grid_glb(n: u32) -> Vec<u8> {
        emit_glb(&LodModel {
            root_name: "grid".to_string(),
            primitives: vec![grid_primitive(n)],
            materials: vec![material("m", AlphaClass::Opaque)],
            images: Vec::new(),
            log: Vec::new(),
        })
        .unwrap()
    }

    // a thin tall landmark with per-segment uv islands, the shape class the
    // pre-fix flags flattened; pins that a 10x ratio never costs the skyline
    fn append_spire(prim: &mut LodPrimitive, cx: f32, cz: f32, top: f32, segments: u32) {
        let w = 0.1f32;
        for (dx, dz) in [(w, 0.0), (0.0, w)] {
            for k in 0..segments {
                let y0 = top * k as f32 / segments as f32;
                let y1 = top * (k + 1) as f32 / segments as f32;
                let base = prim.positions.len() as u32;
                for y in [y0, y1] {
                    prim.positions.push([cx - dx, y, cz - dz]);
                    prim.positions.push([cx + dx, y, cz + dz]);
                    prim.normals.push([dz / w, 0.0, dx / w]);
                    prim.normals.push([dz / w, 0.0, dx / w]);
                }
                let u = (k % 7) as f32 / 8.0;
                let v = (k % 5) as f32 / 6.0;
                prim.uvs.extend_from_slice(&[
                    [u, v],
                    [u + 0.1, v],
                    [u, v + 0.1],
                    [u + 0.1, v + 0.1],
                ]);
                prim.indices.extend_from_slice(&[
                    base,
                    base + 2,
                    base + 1,
                    base + 1,
                    base + 2,
                    base + 3,
                ]);
            }
        }
    }

    fn scattered_quads(material: usize, count: u32, size: f32) -> LodPrimitive {
        let mut prim = LodPrimitive {
            material,
            ..Default::default()
        };
        for k in 0..count {
            let x = (k % 12) as f32 * 0.8;
            let z = (k / 12) as f32 * 0.8;
            let base = prim.positions.len() as u32;
            for (dx, dy) in [(0.0, 0.0), (size, 0.0), (0.0, size), (size, size)] {
                prim.positions.push([x + dx, 1.0 + dy, z]);
                prim.normals.push([0.0, 0.0, 1.0]);
                prim.uvs.push([dx / size, dy / size]);
            }
            prim.indices.extend_from_slice(&[
                base,
                base + 2,
                base + 1,
                base + 1,
                base + 2,
                base + 3,
            ]);
        }
        prim
    }

    #[cfg(unix)]
    fn fake_gltfpack(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("gltfpack");
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[cfg(unix)]
    #[test]
    fn resolution_order_flag_env_path() {
        let dir = temp_dir("resolve");
        let on_path = fake_gltfpack(&dir);
        let path_var = std::ffi::OsString::from(dir.as_os_str());

        let flag = Path::new("/from/flag/gltfpack");
        let env = std::ffi::OsString::from("/from/env/gltfpack");
        assert_eq!(
            resolve_from(Some(flag), Some(&env), Some(&path_var)).unwrap(),
            flag
        );
        assert_eq!(
            resolve_from(None, Some(&env), Some(&path_var)).unwrap(),
            PathBuf::from("/from/env/gltfpack")
        );
        assert_eq!(resolve_from(None, None, Some(&path_var)).unwrap(), on_path);
        let empty_env = std::ffi::OsString::new();
        assert_eq!(
            resolve_from(None, Some(&empty_env), Some(&path_var)).unwrap(),
            on_path
        );

        let plain = dir.join("sub");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("gltfpack"), "not executable").unwrap();
        let miss_var = std::ffi::OsString::from(plain.as_os_str());
        let err = resolve_from(None, None, Some(&miss_var)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("meshoptimizer"), "{msg}");
        assert!(msg.contains("nix-shell"), "{msg}");
    }

    #[test]
    fn rescale_ratio_scales_by_target_over_actual() {
        let r = rescale_ratio(0.1, 1000, 500);
        assert!((r - 0.045).abs() < 1e-12, "{r}");
        let r = rescale_ratio(1.0, 2048, 100);
        assert!((r - 0.0439453125).abs() < 1e-9, "{r}");
        assert_eq!(rescale_ratio(0.001, 10_000_000, 1), 1e-3);
        assert_eq!(rescale_ratio(0.5, 100, 100_000), 1.0);
        assert_eq!(rescale_ratio(0.25, 0, 500), 0.25);
    }

    #[test]
    fn ratio_one_under_cap_is_byte_passthrough() {
        let dir = temp_dir("passthrough");
        let glb = grid_glb(8);
        let input = dir.join("in.glb");
        let output = dir.join("out.glb");
        std::fs::write(&input, &glb).unwrap();
        let report = simplify(
            &input,
            &output,
            1.0,
            Some(1_000_000),
            Path::new("/nonexistent/gltfpack"),
        )
        .unwrap();
        assert!(report.passthrough);
        assert!(!report.unsimplified);
        assert!(report.ratios_run.is_empty());
        assert_eq!(report.tris_before, 128);
        assert_eq!(report.tris_after, 128);
        assert_eq!(std::fs::read(&output).unwrap(), glb);

        let report = simplify(
            &input,
            &output,
            1.0,
            None,
            Path::new("/nonexistent/gltfpack"),
        )
        .unwrap();
        assert!(report.passthrough);
    }

    #[test]
    fn passthrough_copies_byte_identical_without_gltfpack() {
        let dir = temp_dir("purepass");
        let glb = grid_glb(6);
        let input = dir.join("in.glb");
        let output = dir.join("out.glb");
        std::fs::write(&input, &glb).unwrap();
        let report = passthrough(&input, &output).unwrap();
        assert!(report.passthrough);
        assert!(!report.unsimplified);
        assert!(report.ratios_run.is_empty());
        assert_eq!(report.tris_before, report.tris_after);
        assert_eq!(report.tris_before, 72);
        assert_eq!(std::fs::read(&output).unwrap(), glb);
        assert!(!report.summary().contains("UNSIMPLIFIED"));
    }

    #[test]
    fn allow_unsimplified_copies_verbatim() {
        let dir = temp_dir("unsimplified");
        let glb = grid_glb(4);
        let input = dir.join("in.glb");
        let output = dir.join("out.glb");
        std::fs::write(&input, &glb).unwrap();
        let report = copy_unsimplified(&input, &output).unwrap();
        assert!(report.unsimplified);
        assert!(report.passthrough);
        assert_eq!(report.tris_before, report.tris_after);
        assert_eq!(report.tris_before, 32);
        assert_eq!(std::fs::read(&output).unwrap(), glb);
        assert!(report.summary().contains("UNSIMPLIFIED"));
    }

    #[test]
    fn gltfpack_reduces_grid_and_output_reparses() {
        let Ok(bin) = resolve_gltfpack(None) else {
            eprintln!("SKIP: gltfpack not resolvable ({GLTFPACK_NIX_RECIPE})");
            return;
        };
        let dir = temp_dir("reduce");
        let glb = grid_glb(32);
        let input = dir.join("in.glb");
        let output = dir.join("out.glb");
        std::fs::write(&input, &glb).unwrap();

        let report = simplify(&input, &output, 0.25, None, &bin).unwrap();
        assert_eq!(report.tris_before, 2048);
        assert!(report.tris_after > 0);
        assert!(
            report.tris_after < report.tris_before,
            "{} !< {}",
            report.tris_after,
            report.tris_before
        );
        assert_eq!(report.ratios_run, vec![0.25]);

        let capped = simplify(&input, &output, 1.0, Some(100), &bin).unwrap();
        assert!(capped.tris_after <= 100, "{}", capped.tris_after);
        assert!(capped.tris_after > 0);
        assert!(capped.ratios_run.len() >= 2, "{:?}", capped.ratios_run);
    }

    #[test]
    fn simplify_args_are_feature_preserving() {
        assert_eq!(simplify_args(0.1, false, None), vec!["-si", "0.1", "-noq"]);
        assert_eq!(
            simplify_args(0.25, true, None),
            vec!["-si", "0.25", "-sa", "-noq"]
        );
        assert_eq!(
            simplify_args(0.1, false, Some(CLASS_RESCUE_ERROR_LIMIT)),
            vec!["-si", "0.1", "-se", "0.001", "-noq"]
        );
        for lever in ["-sp", "-sa"] {
            assert!(
                !simplify_args(0.1, false, None).iter().any(|a| a == lever),
                "{lever} must not be in the default invocation"
            );
        }
    }

    #[test]
    fn tall_thin_feature_survives_ratio_simplify() {
        let Ok(bin) = resolve_gltfpack(None) else {
            eprintln!("SKIP: gltfpack not resolvable ({GLTFPACK_NIX_RECIPE})");
            return;
        };
        let dir = temp_dir("spire");
        let mut prim = grid_primitive(32);
        append_spire(&mut prim, 5.0, 5.0, 17.0, 40);
        let glb = emit_glb(&LodModel {
            root_name: "spire".to_string(),
            primitives: vec![prim],
            materials: vec![material("m", AlphaClass::Opaque)],
            images: Vec::new(),
            log: Vec::new(),
        })
        .unwrap();
        let input = dir.join("in.glb");
        let output = dir.join("out.glb");
        std::fs::write(&input, &glb).unwrap();

        let report = simplify(&input, &output, 0.1, None, &bin).unwrap();
        assert!(report.tris_after < report.tris_before);
        assert!(report.rescued_classes.is_empty(), "{report:?}");
        let out = glb_model(&output).unwrap();
        let (_, mx) = out.bounds();
        assert!(
            mx[1] >= 16.0,
            "spire pruned: max y {} after {:?}",
            mx[1],
            report
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vanished_material_class_is_rescued() {
        let Ok(bin) = resolve_gltfpack(None) else {
            eprintln!("SKIP: gltfpack not resolvable ({GLTFPACK_NIX_RECIPE})");
            return;
        };
        let dir = temp_dir("rescue");
        let materials = vec![
            material("op", AlphaClass::Opaque),
            material("bl", AlphaClass::Blend),
        ];
        let full = LodModel {
            root_name: "rescue".to_string(),
            primitives: vec![grid_primitive(32), scattered_quads(1, 150, 0.5)],
            materials: materials.clone(),
            images: Vec::new(),
            log: Vec::new(),
        };
        let lost = LodModel {
            root_name: "rescue".to_string(),
            primitives: vec![grid_primitive(8)],
            materials,
            images: Vec::new(),
            log: Vec::new(),
        };
        let input = dir.join("in.glb");
        let output = dir.join("out.glb");
        std::fs::write(&input, emit_glb(&full).unwrap()).unwrap();
        std::fs::write(&output, emit_glb(&lost).unwrap()).unwrap();

        let mut report = SimplifyReport::default();
        rescue_lost_classes(&input, &output, 0.1, &bin, &mut report).unwrap();
        assert_eq!(report.rescued_classes, vec!["bl".to_string()]);
        let out = glb_model(&output).unwrap();
        let by_mat = tris_by_material(&out);
        assert_eq!(by_mat.get("op").copied().unwrap_or(0), 128);
        assert!(by_mat.get("bl").copied().unwrap_or(0) > 0, "{by_mat:?}");
        assert_eq!(report.tris_after, out.total_tris());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn deadline_kills_hung_subprocess_within_budget() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let t = std::time::Instant::now();
        let err = run_with_deadline(cmd, Some(Duration::from_millis(300)), "sleep 30").unwrap_err();
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "kill took {:?}",
            t.elapsed()
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("deadline"), "{msg}");
        assert!(msg.contains(SUBPROC_TIMEOUT_ENV), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn deadline_passthrough_captures_output() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out-line; echo err-line >&2; exit 0"]);
        let out = run_with_deadline(cmd, Some(Duration::from_secs(30)), "sh echo").unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "out-line");
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "err-line");

        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 3"]);
        let out = run_with_deadline(cmd, None, "sh exit 3").unwrap();
        assert!(!out.status.success());
    }
}
