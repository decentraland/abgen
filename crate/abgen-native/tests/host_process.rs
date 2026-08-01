//! Drives the real `abgen-host` binary over its stdio protocol.
//!
//! The point of the out-of-process mode is that a hostile asset cannot reach
//! the caller, so the tests that matter are the ones where the helper misuses
//! itself and the harness survives to assert about it.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use abgen_core::export::{InputBuilder, Kind};

const FRAME_DONE: u32 = 0xFFFF_FFFF;
#[allow(dead_code)]
const EXIT_PROTOCOL: i32 = 64;
#[allow(dead_code)]
const EXIT_LIMIT: i32 = 65;

fn host_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().expect("test exe path");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join(if cfg!(windows) {
        "abgen-host.exe"
    } else {
        "abgen-host"
    })
}

#[derive(Default, Debug)]
struct Reply {
    events: Vec<String>,
    outputs: Vec<(String, usize)>,
    errors: Vec<String>,
    manifest: Option<String>,
    code: Option<i32>,
}

fn parse_frames(buf: &[u8]) -> Reply {
    let mut r = Reply::default();
    let mut off = 0usize;
    while off + 8 <= buf.len() {
        let kind = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        if kind == FRAME_DONE {
            r.code = Some(i32::from_le_bytes(
                buf[off + 4..off + 8].try_into().unwrap(),
            ));
            break;
        }
        let len = u32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap()) as usize;
        off += 8;
        if off + len > buf.len() {
            break;
        }
        let payload = &buf[off..off + len];
        off += len;
        match kind {
            k if k == Kind::Json as u32 => r.events.push(String::from_utf8_lossy(payload).into()),
            k if k == Kind::Output as u32 => {
                if let Some((name, data)) = abgen_core::export::split_output(payload) {
                    r.outputs.push((name, data.len()));
                }
            }
            k if k == Kind::Error as u32 => r.errors.push(String::from_utf8_lossy(payload).into()),
            k if k == Kind::Manifest as u32 => {
                r.manifest = Some(String::from_utf8_lossy(payload).into())
            }
            _ => {}
        }
    }
    r
}

/// Runs one request through a freshly spawned helper.
fn run_host(request: &[u8], extra: &[&str]) -> (Reply, Option<i32>) {
    let mut child = Command::new(host_bin())
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn abgen-host");

    {
        let mut stdin = child.stdin.take().expect("stdin");
        stdin
            .write_all(&(request.len() as u32).to_le_bytes())
            .expect("write len");
        stdin.write_all(request).expect("write request");
    }

    let mut out = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_end(&mut out)
        .expect("read stdout");
    let status = child.wait().expect("wait");
    (parse_frames(&out), status.code())
}

fn quad_glb() -> Vec<u8> {
    let p = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../abgen-wasm/test/fixtures/normal-quad.glb"
    );
    std::fs::read(p).expect("fixture glb")
}

#[test]
fn converts_a_glb_across_the_process_boundary() {
    let blob = InputBuilder::new()
        .file("model.glb", quad_glb())
        .platform("windows")
        .build();

    let (reply, exit) = run_host(&blob, &[]);
    assert_eq!(exit, Some(0), "helper should exit 0");
    assert_eq!(reply.code, Some(0), "trailer should carry the exit code");
    assert!(
        reply.errors.is_empty(),
        "unexpected errors: {:?}",
        reply.errors
    );
    assert_eq!(reply.outputs.len(), 1, "expected one bundle");
    assert!(reply.outputs[0].1 > 0, "bundle should carry bytes");
    let name = &reply.outputs[0].0;
    assert!(
        name.ends_with("_windows"),
        "bundle name should carry the platform suffix, got {name:?}"
    );
    let manifest = reply.manifest.clone().expect("manifest frame");
    assert!(
        manifest.contains(name.as_str()),
        "the manifest must list the bundle it emitted: {manifest}"
    );
    assert!(!reply.events.is_empty(), "expected progress events");

    let manifest = reply.manifest.expect("manifest frame");
    assert!(
        manifest.contains("v-abgen-host"),
        "manifest should identify the host: {manifest}"
    );
    assert!(manifest.contains("\"exitCode\":0"), "{manifest}");
}

/// The isolation claim, stated as a test: garbage in produces a reported
/// failure and a live harness, not a dead one.
#[test]
fn a_corrupt_asset_does_not_take_the_caller_down() {
    let blob = InputBuilder::new()
        .file("evil.glb", vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x01])
        .platform("windows")
        .build();

    let (reply, exit) = run_host(&blob, &[]);
    assert_eq!(exit, Some(0), "a bad asset is a file error, not a crash");
    assert!(reply.outputs.is_empty(), "nothing should be emitted");
    assert!(
        reply.events.iter().any(|e| e.contains("file-error")),
        "expected a file-error event, got {:?}",
        reply.events
    );
}

#[test]
fn a_malformed_request_is_rejected_not_guessed_at() {
    let (reply, _) = run_host(&[0xff, 0xff, 0xff, 0xff, 0x00], &[]);
    assert_eq!(reply.code, Some(1));
    assert_eq!(reply.errors, vec!["malformed input blob".to_string()]);
}

/// A memory cap has to actually bind, or the process boundary is only a
/// separation and not a bound.
///
/// It binds only because the helper re-executes after `setrlimit`: applied
/// in-process, the cap arrives after mimalloc has reserved its arenas and an
/// 8 MB limit still converts a glb.
///
/// Linux-gated, and the gate matters: on macOS this assertion passes for the
/// wrong reason, because the helper exits non-zero having *refused* to apply
/// a cap it cannot enforce.
#[cfg(target_os = "linux")]
#[test]
fn a_memory_cap_binds() {
    let blob = InputBuilder::new()
        .file("model.glb", quad_glb())
        .platform("windows")
        .build();

    let (_reply, exit) = run_host(&blob, &["--max-memory-mb", "32"]);
    assert_ne!(exit, Some(0), "a 32 MB cap should stop the conversion");
    assert_ne!(exit, Some(EXIT_LIMIT), "the cap must bind, not be refused");
    assert_ne!(exit, Some(EXIT_PROTOCOL), "not a protocol error");
}

/// The cap must still leave a workable process at a sane size. This is the
/// regression guard for the interaction that broke it: rayon defaults to one
/// worker per core, and on a many-core machine their stacks alone exhaust an
/// otherwise generous RLIMIT_AS before any conversion starts.
#[cfg(target_os = "linux")]
#[test]
fn a_generous_cap_still_converts() {
    let blob = InputBuilder::new()
        .file("model.glb", quad_glb())
        .platform("windows")
        .build();

    let (reply, exit) = run_host(&blob, &["--max-memory-mb", "2048"]);
    assert_eq!(
        exit,
        Some(0),
        "2 GB should be ample; if this fails the pool is not being bounded \
         alongside the cap"
    );
    assert_eq!(reply.outputs.len(), 1);
}

/// macOS enforces no per-process memory rlimit (measured: RLIMIT_AS/DATA/RSS
/// at 256 MB all let a conversion finish). The helper must therefore refuse
/// the flag rather than accept it and cap nothing.
#[cfg(target_os = "macos")]
#[test]
fn a_memory_cap_is_refused_rather_than_silently_ignored() {
    let blob = InputBuilder::new()
        .file("model.glb", quad_glb())
        .platform("windows")
        .build();

    let (reply, exit) = run_host(&blob, &["--max-memory-mb", "2048"]);
    assert_eq!(
        exit,
        Some(EXIT_LIMIT),
        "should exit EXIT_LIMIT, not convert uncapped"
    );
    assert!(
        reply.outputs.is_empty(),
        "refusing the cap must not produce an unbounded conversion"
    );
}

/// The Windows counterpart of the Linux cap tests. The thresholds differ by
/// orders of magnitude on purpose: a job object limits *committed* memory
/// while RLIMIT_AS limits *reserved* address space, so measured on v16 this
/// binds at 1-4 MB and passes from 8 MB up, where Linux wants gigabytes.
#[cfg(windows)]
#[test]
fn a_job_object_cap_binds_but_leaves_a_workable_process() {
    let blob = InputBuilder::new()
        .file("model.glb", quad_glb())
        .platform("windows")
        .build();

    let (_tiny, tiny_exit) = run_host(&blob, &["--max-memory-mb", "2"]);
    assert_ne!(
        tiny_exit,
        Some(0),
        "a 2 MB job-object cap should stop the run"
    );
    assert_ne!(
        tiny_exit,
        Some(EXIT_LIMIT),
        "the cap must bind, not be refused"
    );
    assert_ne!(tiny_exit, Some(EXIT_PROTOCOL), "not a protocol error");

    let (ample, ample_exit) = run_host(&blob, &["--max-memory-mb", "2048"]);
    assert_eq!(ample_exit, Some(0), "2 GB should be ample");
    assert_eq!(
        ample.outputs.len(),
        1,
        "capped run should still emit its bundle"
    );
}

#[test]
fn reports_its_version() {
    let out = Command::new(host_bin())
        .arg("--version")
        .output()
        .expect("run --version");
    assert!(out.status.success());
    let v = String::from_utf8_lossy(&out.stdout);
    assert_eq!(v.trim(), env!("CARGO_PKG_VERSION"));
}
