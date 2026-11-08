//! The scene sandbox end to end: this binary is both the parent and, re-executed with the
//! child argument, the sandbox child. That is why it runs without the libtest harness.

use abgen::lodgen::scenerun::{sandbox, EngineLimits, SandboxedEngine, SceneEngine, SceneJob};
use std::time::Duration;

fn job(code: &str, deadline: Option<Duration>) -> SceneJob {
    SceneJob {
        code: code.to_string(),
        main_crdt: None,
        read_file: Box::new(|name| match name {
            "a.bin" => Ok((vec![7, 9], "hhash".to_string())),
            other => anyhow::bail!("scene content does not list file {other}"),
        }),
        limits: EngineLimits {
            deadline,
            ..EngineLimits::default()
        },
    }
}

fn captures_renderer_stream_and_serves_listed_files() {
    let code = r#"
        var f = __abgen.readFile('a.bin');
        var b = new Uint8Array(f.content);
        var unlisted = 0;
        try { __abgen.readFile('missing.bin'); } catch (e) { unlisted = 1; }
        __abgen.sendToRenderer(new Uint8Array([b[0], b[1], f.hash.length, unlisted]));
    "#;
    let out = SandboxedEngine.run_capture(job(code, None)).unwrap();
    assert!(out.sent);
    assert_eq!(out.stream, vec![7, 9, 5, 1]);
    assert!(out.eval_error.is_none());
}

fn reports_eval_errors() {
    let out = SandboxedEngine
        .run_capture(job("throw new Error('nope')", None))
        .unwrap();
    assert!(!out.sent);
    assert!(out.eval_error.unwrap_or_default().contains("nope"));
}

fn enforces_the_deadline() {
    let err = SandboxedEngine
        .run_capture(job("for(;;){}", Some(Duration::from_millis(300))))
        .unwrap_err();
    assert!(format!("{err:#}").contains("deadline"), "{err:#}");
}

fn child_is_confined() {
    let report = sandbox::self_test().unwrap();
    assert!(report.contains("empty_env=ok"), "{report}");
    #[cfg(target_os = "linux")]
    for probe in [
        "open",
        "openat",
        "socket",
        "mmap_exec",
        "ptrace",
        "clone",
        "execve",
    ] {
        assert!(
            report.contains(&format!("{probe}=ok")),
            "{probe} not refused:\n{report}"
        );
    }
}

fn main() {
    abgen::lodgen::scenerun::run_sandbox_child_if_requested();
    // The child must start with an empty environment even when the parent has one.
    std::env::set_var("ABGEN_SANDBOX_TEST_PARENT_VAR", "1");

    let tests: [(&str, fn()); 4] = [
        ("child_is_confined", child_is_confined),
        (
            "captures_renderer_stream_and_serves_listed_files",
            captures_renderer_stream_and_serves_listed_files,
        ),
        ("reports_eval_errors", reports_eval_errors),
        ("enforces_the_deadline", enforces_the_deadline),
    ];
    let mut failed = 0;
    for (name, test) in tests {
        match std::panic::catch_unwind(test) {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                println!("test {name} ... FAILED");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        eprintln!("{failed} test(s) failed");
        std::process::exit(1);
    }
}
