//! The scene sandbox end to end: this binary is both the parent and, re-executed with the
//! child argument, the sandbox child. That is why it runs without the libtest harness.
//!
//! It speaks enough of libtest's CLI for cargo-nextest: `--list --format terse` prints
//! `name: test` per test, and `<name> --exact` runs one.

use abgen::lodgen::scenerun::{sandbox, EngineLimits, SandboxedEngine, SceneEngine, SceneJob};
use std::sync::Mutex;
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
    // The default policy is `required`, so on Linux a passing self-test means the filter
    // is on; a runtime that refuses seccomp would have failed `self_test()` above.
    #[cfg(target_os = "linux")]
    for probe in [
        "seccomp",
        "open",
        "openat",
        "socket",
        "mmap_exec",
        "ptrace",
        "clone",
        "execve",
        "tgkill_other",
    ] {
        assert!(
            report.contains(&format!("{probe}=ok")),
            "{probe} not refused:\n{report}"
        );
    }
}

/// Engine events raised inside the child reach the parent's subscriber at their level.
fn forwards_scene_logs_and_eval_warnings() {
    RECORDED.lock().unwrap().clear();
    let code = "__abgen.log('hola desde la escena'); throw new Error('después del log')";
    let out = SandboxedEngine.run_capture(job(code, None)).unwrap();
    assert!(out
        .eval_error
        .unwrap_or_default()
        .contains("después del log"));
    let lines = RECORDED.lock().unwrap().clone();
    let has =
        |level: &str, needle: &str| lines.iter().any(|(l, m)| l == level && m.contains(needle));
    assert!(
        has("TRACE", "scene sandbox: scene log: hola desde la escena"),
        "{lines:#?}"
    );
    assert!(
        lines.iter().any(|(l, m)| l == "WARN"
            && m.starts_with("scene sandbox: scene eval failed:")
            && m.contains("después del log")),
        "{lines:#?}"
    );
}

/// A parent-side protocol error ends the child instead of waiting on it: the oversized
/// name is refused before the body is read, the pipes close, and the child exits.
fn protocol_errors_do_not_hang_the_parent() {
    let code = "try { __abgen.readFile('a'.repeat(5000)); } catch (e) {} for(;;){}";
    let started = std::time::Instant::now();
    let err = SandboxedEngine.run_capture(job(code, None)).unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "took {:?}",
        started.elapsed()
    );
    assert!(format!("{err:#}").contains("exceeds"), "{err:#}");
}

fn check_runs_one_scene() {
    sandbox::check().unwrap();
}

static RECORDED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Records every event's level and message so the tests can assert on forwarded logs.
struct Recorder;

struct Message(String);

impl tracing::field::Visit for Message {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        }
    }
}

impl tracing::Subscriber for Recorder {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::TRACE)
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut msg = Message(String::new());
        event.record(&mut msg);
        RECORDED
            .lock()
            .unwrap()
            .push((event.metadata().level().to_string(), msg.0));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn main() {
    abgen::lodgen::scenerun::run_sandbox_child_if_requested();

    let tests: [(&str, fn()); 7] = [
        ("child_is_confined", child_is_confined),
        (
            "captures_renderer_stream_and_serves_listed_files",
            captures_renderer_stream_and_serves_listed_files,
        ),
        ("reports_eval_errors", reports_eval_errors),
        ("enforces_the_deadline", enforces_the_deadline),
        (
            "forwards_scene_logs_and_eval_warnings",
            forwards_scene_logs_and_eval_warnings,
        ),
        (
            "protocol_errors_do_not_hang_the_parent",
            protocol_errors_do_not_hang_the_parent,
        ),
        ("check_runs_one_scene", check_runs_one_scene),
    ];

    // libtest-style arguments, as cargo-nextest sends them.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--list") {
        if !args.iter().any(|a| a == "--ignored") {
            for (name, _) in tests {
                println!("{name}: test");
            }
        }
        return;
    }
    let exact = args.iter().any(|a| a == "--exact");
    let mut filters: Vec<&str> = Vec::new();
    let mut skip_value = false;
    for arg in &args {
        if skip_value {
            skip_value = false;
        } else if arg == "--format" || arg == "--skip" {
            skip_value = true;
        } else if !arg.starts_with('-') {
            filters.push(arg);
        }
    }
    let selected = |name: &str| {
        filters.is_empty()
            || filters
                .iter()
                .any(|f| if exact { *f == name } else { name.contains(f) })
    };

    // The child must start with an empty environment even when the parent has one.
    std::env::set_var("ABGEN_SANDBOX_TEST_PARENT_VAR", "1");
    tracing::subscriber::set_global_default(Recorder).expect("no subscriber yet");

    let mut failed = 0;
    for (name, test) in tests {
        if !selected(name) {
            continue;
        }
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
