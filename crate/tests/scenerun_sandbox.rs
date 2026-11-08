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

/// Set by the two "runtime refuses seccomp" tests on the process they spawn: that process
/// installs a filter that fails `prctl(PR_SET_NO_NEW_PRIVS)` with `EPERM`, which every
/// sandbox child it then starts inherits, exactly as on AWS Lambda.
#[cfg(target_os = "linux")]
const SCENARIO_ENV: &str = "ABGEN_SANDBOX_TEST_SCENARIO";

#[cfg(target_os = "linux")]
fn deny_no_new_privs_in_this_process() {
    // Classic BPF over `seccomp_data`: nr at 0, args at 16 (low word on little-endian).
    const LD_W_ABS: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const RET_K: u16 = 0x06;
    const RET_ALLOW: u32 = 0x7fff_0000;
    const RET_ERRNO: u32 = 0x0005_0000;
    let stmt = |code: u16, k: u32| libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |code: u16, k: u32, jt: u8, jf: u8| libc::sock_filter { code, jt, jf, k };
    let prog = [
        stmt(LD_W_ABS, 0),
        jump(JEQ_K, libc::SYS_prctl as u32, 0, 3),
        stmt(LD_W_ABS, 16),
        jump(JEQ_K, libc::PR_SET_NO_NEW_PRIVS as u32, 0, 1),
        stmt(RET_K, RET_ERRNO | libc::EPERM as u32),
        stmt(RET_K, RET_ALLOW),
    ];
    let fprog = libc::sock_fprog {
        len: prog.len() as u16,
        filter: prog.as_ptr() as *mut libc::sock_filter,
    };
    // SAFETY: prctl and seccomp with a valid, live program; this process is a throwaway.
    unsafe {
        assert_eq!(
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0),
            0,
            "no_new_privs: {}",
            std::io::Error::last_os_error()
        );
        // SECCOMP_SET_MODE_FILTER = 1, SECCOMP_FILTER_FLAG_TSYNC = 1.
        assert_eq!(
            libc::syscall(libc::SYS_seccomp, 1, 1, &fprog as *const libc::sock_fprog),
            0,
            "seccomp: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Runs one scenario in this (throwaway) process; exits 0 when it behaved as expected.
#[cfg(target_os = "linux")]
fn run_scenario(scenario: &str) -> ! {
    deny_no_new_privs_in_this_process();
    let code = match scenario {
        "no-seccomp-auto" => {
            std::env::set_var(sandbox::SANDBOX_SECCOMP_ENV, "auto");
            match sandbox::check() {
                Ok(report)
                    if report.contains("seccomp=unavailable(PR_SET_NO_NEW_PRIVS")
                        && !report.contains("open=") =>
                {
                    println!("{report}");
                    0
                }
                Ok(report) => {
                    eprintln!("unexpected report:\n{report}");
                    1
                }
                Err(e) => {
                    eprintln!("check failed under auto: {e:#}");
                    1
                }
            }
        }
        "no-seccomp-required" => match sandbox::check() {
            Err(e) if format!("{e:#}").contains("ABGEN_SANDBOX_SECCOMP=auto") => {
                println!("{e:#}");
                0
            }
            Err(e) => {
                eprintln!("wrong error under required: {e:#}");
                1
            }
            Ok(report) => {
                eprintln!("check passed without seccomp under required:\n{report}");
                1
            }
        },
        other => {
            eprintln!("unknown scenario {other}");
            2
        }
    };
    std::process::exit(code);
}

#[cfg(target_os = "linux")]
fn scenario(name: &str) -> String {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .env(SCENARIO_ENV, name)
        .env_remove(sandbox::SANDBOX_SECCOMP_ENV)
        .stderr(std::process::Stdio::inherit())
        .output()
        .expect("spawn scenario process");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "scenario {name} failed ({}):\n{stdout}",
        out.status
    );
    stdout
}

/// Where the runtime refuses `PR_SET_NO_NEW_PRIVS` (Lambda), `auto` still runs a scene and
/// reports the missing filter instead of probing for it.
fn auto_runs_scenes_where_seccomp_is_refused() {
    #[cfg(target_os = "linux")]
    {
        let report = scenario("no-seccomp-auto");
        assert!(report.contains("empty_env=ok"), "{report}");
    }
}

/// Same host, default policy: refused, with the hint, as a definitive verdict.
fn required_refuses_scenes_where_seccomp_is_refused() {
    #[cfg(target_os = "linux")]
    {
        let err = scenario("no-seccomp-required");
        assert!(err.contains("seccomp is unavailable here"), "{err}");
    }
}

fn main() {
    abgen::lodgen::scenerun::run_sandbox_child_if_requested();
    #[cfg(target_os = "linux")]
    if let Ok(name) = std::env::var(SCENARIO_ENV) {
        run_scenario(&name);
    }

    let tests: [(&str, fn()); 9] = [
        ("child_is_confined", child_is_confined),
        (
            "auto_runs_scenes_where_seccomp_is_refused",
            auto_runs_scenes_where_seccomp_is_refused,
        ),
        (
            "required_refuses_scenes_where_seccomp_is_refused",
            required_refuses_scenes_where_seccomp_is_refused,
        ),
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
