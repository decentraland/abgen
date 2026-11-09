//! Runs each scene script in its own short-lived child process.
//!
//! * The parent re-executes its own binary with [`CHILD_ARG`] and an empty environment,
//!   keeps only stdin/stdout/stderr open in the child, and has the child killed if the
//!   parent exits. The parent is marked non-dumpable.
//! * The child restricts itself with a seccomp allowlist before reading the job: I/O on
//!   the descriptors it already has, memory management without `PROT_EXEC`, futexes,
//!   clocks and exit. Other syscalls return `EPERM`.
//! * `readFile` is served by the parent over the pipe through the same content-listed
//!   closure as the in-process engine; with the filter on, the child cannot reach the
//!   network or the filesystem at all. The parent accepts only bounded, typed frames back.
//! * The child has no tracing subscriber of its own: engine and driver events (scene logs,
//!   eval failures) travel to the parent as bounded log frames, which the parent re-emits
//!   at their level, so `RUST_LOG` keeps working as it did in-process.
//! * A deadline kills the child if the engine's own interrupt does not stop it.
//!
//! The first job in a process runs [`self_test`]; if the child setup is not in effect,
//! jobs fail instead of running. A self-test that could not run at all (spawn failure) is
//! retried on the next job rather than cached. Non-Linux hosts get the separate process
//! and empty environment without seccomp, parent-death kill or resource limits, with a
//! warning logged once: there "sandbox" means fault isolation, not confinement. "Empty"
//! allows for the variables the platform injects on its own ([`PLATFORM_INJECTED_ENV`]).
//!
//! Some Linux runtimes refuse seccomp to the processes they host: AWS Lambda returns
//! `EPERM` from `prctl(PR_SET_NO_NEW_PRIVS)`. By default that refuses every scene
//! (`seccomp=unavailable` in the self-test report). [`SANDBOX_SECCOMP_ENV`]`=auto` accepts
//! it: scenes run in the separate process with the empty environment, `RLIMIT_DATA`,
//! `RLIMIT_NPROC=0`, its own process group and parent-death kill, without the syscall
//! filter, and the parent logs a warning once and counts it in
//! `abgen_scene_sandbox_seccomp_total{state="unavailable"}`. Only the runtime's refusal
//! (`EPERM`, `EACCES`, `ENOSYS`) is tolerated; a filter the kernel rejects for any other
//! reason still fails closed, and so does a parent that `PR_SET_DUMPABLE` could not make
//! non-dumpable, under either policy: without the filter it is what keeps an escaped
//! scene out of `/proc/<ppid>/environ`. Only the Lambda image sets `auto`. Under it, a
//! QuickJS escape has the uid's ordinary access (files, sockets, `execve`); what it does
//! not get is a second process, the parent's memory, or a life after the parent's.
//!
//! Without the filter the child also lowers `RLIMIT_NOFILE` to the descriptors it already
//! has, so it cannot open files or sockets, and the self-test then proves that opening a
//! file, opening a socket, starting a process and reading the parent's memory are all
//! refused (`nofile_open`, `nofile_socket`, `nproc_clone`, `parent_mem`). Root is exempt
//! from `RLIMIT_NPROC`, so without the filter a root process fails the self-test.
//!
//! A process that holds nothing worth protecting from the scene (the scene runner function,
//! whose only permission is writing its own logs) can call [`accept_dumpable_parent`]: a
//! parent that stays dumpable is then a warning instead of a failure, provided the child
//! still cannot read or write the parent's memory (`parent_mem=ok`). Nothing else calls it.
//!
//! Concurrent children are capped at `default_file_concurrency()`, as the in-process pool
//! was; callers' LOD limits already keep them below that today.
//!
//! When the binary runs under an explicit dynamic loader (the Linux release archive's
//! `ld.so --library-path … abgen.bin` wrapper), `current_exe()` is the loader, so the
//! wrapper exports [`SANDBOX_LOADER_ENV`], [`SANDBOX_LIBPATH_ENV`] and [`SANDBOX_BIN_ENV`]
//! and the child is started the same way. `check()` backs `abgen --scene-sandbox-check`,
//! which the release sanity step runs.

use super::{CaptureOutcome, EngineLimits, SceneEngine, SceneJob};
use anyhow::{anyhow, bail, Context, Result};
use std::ffi::OsStr;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

/// argv[1] that turns any abgen binary into a scene sandbox child. Binaries that can run
/// scenes call [`run_child_if_requested`] first thing in `main`.
pub const CHILD_ARG: &str = "__abgen-scenerun-sandbox-child";

/// Set by a launcher that runs the binary through an explicit loader: the real binary, the
/// loader, and its `--library-path`. Read by the parent only; the child's env is empty.
pub const SANDBOX_BIN_ENV: &str = "ABGEN_SANDBOX_BIN";
pub const SANDBOX_LOADER_ENV: &str = "ABGEN_SANDBOX_LOADER";
pub const SANDBOX_LIBPATH_ENV: &str = "ABGEN_SANDBOX_LIBPATH";

/// Read by the parent: `required` (the default) refuses to run scenes on Linux when the
/// child cannot install its seccomp filter; `auto` runs them anyway in the separate process
/// (empty environment, resource limits, parent-death kill) and warns once. The policy
/// reaches the child as argv[2], since the child's environment is empty.
pub const SANDBOX_SECCOMP_ENV: &str = "ABGEN_SANDBOX_SECCOMP";
const CHILD_SECCOMP_AUTO_ARG: &str = "seccomp=auto";
const CHILD_SECCOMP_REQUIRED_ARG: &str = "seccomp=required";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SeccompPolicy {
    Required,
    Auto,
}

impl SeccompPolicy {
    fn child_arg(self) -> &'static str {
        match self {
            SeccompPolicy::Required => CHILD_SECCOMP_REQUIRED_ARG,
            SeccompPolicy::Auto => CHILD_SECCOMP_AUTO_ARG,
        }
    }

    fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("auto") {
            Some(SeccompPolicy::Auto)
        } else if value.is_empty() || value.eq_ignore_ascii_case("required") {
            Some(SeccompPolicy::Required)
        } else {
            None
        }
    }
}

/// The parent's policy, read once. An unrecognised value is `required` with a warning.
fn seccomp_policy() -> SeccompPolicy {
    static POLICY: OnceLock<SeccompPolicy> = OnceLock::new();
    *POLICY.get_or_init(|| match std::env::var(SANDBOX_SECCOMP_ENV) {
        Err(_) => SeccompPolicy::Required,
        Ok(value) => SeccompPolicy::parse(&value).unwrap_or_else(|| {
            tracing::warn!(
                "scene sandbox: {SANDBOX_SECCOMP_ENV}={value:?} is neither `required` nor \
                 `auto`; using `required`"
            );
            SeccompPolicy::Required
        }),
    })
}

/// Whether the child's seccomp filter is on. Reported as `seccomp=ok` or
/// `seccomp=unavailable(reason)` in the self-test report.
enum SeccompState {
    /// Only Linux has a filter to install; elsewhere the state is always `Unavailable`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Installed,
    Unavailable(String),
}

const MAGIC: &[u8; 8] = b"ABSRUN01";

// Parent -> child.
const T_JOB: u8 = 1;
const T_FILE_OK: u8 = 2;
const T_FILE_ERR: u8 = 3;
const T_SELF_TEST: u8 = 4;
// Child -> parent.
const T_READY: u8 = 10;
const T_READ_FILE: u8 = 11;
const T_DONE: u8 = 12;
const T_FAILED: u8 = 13;
const T_SELF_TEST_REPORT: u8 = 14;
/// `[level u8][utf-8 message]`, a tracing event from inside the child.
const T_LOG: u8 = 15;

/// Bounds on what the parent accepts from the child.
const MAX_CHILD_FRAME: usize = 256 << 20;
const MAX_FILE_NAME: usize = 4096;
const MAX_CHILD_TEXT: usize = 4096;
/// `readFile` calls per scene. The in-process engine had no such cap; a scene that reads
/// more than this is treated as misbehaving and fails.
const MAX_FILE_READS: usize = 10_000;
/// Log frames per scene; the child stops forwarding (and the parent stops accepting) past
/// this, so a chatty scene cannot flood the parent's logs.
const MAX_LOG_LINES: usize = 500;
/// Bound on what the child accepts from the parent (scene code, main.crdt, files). The
/// parent enforces it before sending, so the pipe never goes out of sync over size.
const MAX_PARENT_FRAME: usize = 1 << 30;

/// `RLIMIT_DATA` for the child: a backstop over the QuickJS memory limit for the host-side
/// buffers (file copies, renderer stream) that the engine limit does not cover.
#[cfg(target_os = "linux")]
const CHILD_DATA_LIMIT: u64 = 4 << 30;

/// Slack over the engine's own deadline before the parent kills the child outright, and
/// the hard cap when the job has no deadline.
const KILL_GRACE: Duration = Duration::from_secs(30);
const KILL_WITHOUT_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// Runs each scene in a fresh sandboxed child process. See the module docs.
pub struct SandboxedEngine;

impl SceneEngine for SandboxedEngine {
    fn run_capture(&self, job: SceneJob) -> Result<CaptureOutcome> {
        ensure_sandbox_works()?;
        let _slot = ChildSlot::acquire();
        run_in_child(Request::Job(job))?.into_capture()
    }
}

/// Caps concurrent scene children at the same count the in-process pool had.
struct ChildSlot;

struct ChildGate {
    running: Mutex<usize>,
    freed: std::sync::Condvar,
}

fn child_gate() -> &'static ChildGate {
    static GATE: OnceLock<ChildGate> = OnceLock::new();
    GATE.get_or_init(|| ChildGate {
        running: Mutex::new(0),
        freed: std::sync::Condvar::new(),
    })
}

impl ChildSlot {
    fn acquire() -> Self {
        let max = crate::clihelp::default_file_concurrency().max(1);
        let gate = child_gate();
        let mut running = lock(&gate.running);
        while *running >= max {
            running = gate.freed.wait(running).unwrap_or_else(|e| e.into_inner());
        }
        *running += 1;
        ChildSlot
    }
}

impl Drop for ChildSlot {
    fn drop(&mut self) {
        let gate = child_gate();
        *lock(&gate.running) -= 1;
        gate.freed.notify_one();
    }
}

enum Request {
    Job(SceneJob),
    SelfTest,
}

enum Response {
    Capture(CaptureOutcome),
    SelfTest(String),
}

impl Response {
    fn into_capture(self) -> Result<CaptureOutcome> {
        match self {
            Response::Capture(c) => Ok(c),
            Response::SelfTest(_) => bail!("scene sandbox answered a job with a self-test report"),
        }
    }
}

/// Spawns one child, runs the self-test in it and checks every probe was refused. Run
/// before the first job; a broken sandbox fails every job instead of quietly running
/// scenes unconfined. Only a verdict is cached: a self-test that could not run (spawn or
/// pipe failure) is reported and tried again on the next job.
fn ensure_sandbox_works() -> Result<()> {
    static VERDICT: Mutex<Option<std::result::Result<(), String>>> = Mutex::new(None);
    let mut verdict = lock(&VERDICT);
    if let Some(cached) = &*verdict {
        return cached
            .clone()
            .map_err(|e| anyhow!("scene sandbox self-test failed, refusing to run scenes: {e}"));
    }
    match self_test_report() {
        Ok(_) => {
            *verdict = Some(Ok(()));
            Ok(())
        }
        Err(SelfTestError::Definitive(e)) => {
            *verdict = Some(Err(e.clone()));
            bail!("scene sandbox self-test failed, refusing to run scenes: {e}")
        }
        Err(SelfTestError::Transient(e)) => {
            Err(e.context("scene sandbox self-test could not run; it is retried on the next scene"))
        }
    }
}

enum SelfTestError {
    /// The child ran and its confinement is not in effect; the same binary on the same
    /// host will keep failing.
    Definitive(String),
    /// The child could not be started or talked to.
    Transient(anyhow::Error),
}

/// Starts a child, has it probe its own confinement and returns its report
/// (`name=ok|FAIL(...)` per line, plus `seccomp=ok|unavailable(...)`). Errors if any probe
/// was not refused, or if seccomp is unavailable and the policy requires it.
pub fn self_test() -> Result<String> {
    self_test_report().map_err(|e| match e {
        SelfTestError::Definitive(e) => anyhow!("{e}"),
        SelfTestError::Transient(e) => e,
    })
}

fn self_test_report() -> std::result::Result<String, SelfTestError> {
    let report = match run_in_child(Request::SelfTest).map_err(SelfTestError::Transient)? {
        Response::SelfTest(r) => r,
        Response::Capture(_) => {
            return Err(SelfTestError::Transient(anyhow!(
                "scene sandbox answered a self-test with a capture"
            )))
        }
    };
    let seccomp_state = if report.contains("\nseccomp=ok") {
        "installed"
    } else {
        "unavailable"
    };
    metrics::counter!("abgen_scene_sandbox_seccomp_total", "state" => seccomp_state).increment(1);
    let dumpable = parent_dumpable_error();
    match evaluate_report(
        &report,
        seccomp_policy(),
        dumpable.as_deref(),
        dumpable_parent_accepted(),
    ) {
        Ok(warnings) => {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                for warning in &warnings {
                    tracing::warn!("scene sandbox: {warning}");
                }
            });
            Ok(report)
        }
        Err(e) => Err(SelfTestError::Definitive(e)),
    }
}

/// What a self-test report means under `policy`. `Ok(warnings)`: scenes may run, each
/// warning worth logging once; `Err`: the sandbox is not in effect and scenes must not run.
///
/// A probe line that is not `=ok` is a failure whatever the policy: it means a filter is
/// on and does not refuse what it should. `seccomp=unavailable(..)` is a failure under
/// `required` on Linux only; elsewhere seccomp never exists. A parent that is still
/// dumpable (`parent_dumpable` names why) is a failure under either policy: without the
/// filter, non-dumpability is what keeps an escaped scene out of `/proc/<ppid>/environ`
/// and `/proc/<ppid>/mem`, which on Lambda hold the function's credentials. With
/// `dumpable_accepted` ([`accept_dumpable_parent`]) it is a warning instead, but only when
/// the report proves the child was refused the parent's memory (`parent_mem=ok`).
fn evaluate_report(
    report: &str,
    policy: SeccompPolicy,
    parent_dumpable: Option<&str>,
    dumpable_accepted: bool,
) -> std::result::Result<Vec<String>, String> {
    let mut failed = Vec::new();
    let mut seccomp_unavailable = None;
    for line in report.lines() {
        if line.ends_with("=ok") {
            continue;
        }
        match line
            .strip_prefix("seccomp=unavailable(")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            Some(reason) => seccomp_unavailable = Some(reason.to_string()),
            None => failed.push(line),
        }
    }
    if !failed.is_empty() {
        return Err(format!("sandbox probes not refused: {}", failed.join(", ")));
    }
    let mut warnings = Vec::new();
    if let Some(why) = parent_dumpable {
        if !dumpable_accepted {
            return Err(format!("parent process is still dumpable ({why})"));
        }
        if !report.lines().any(|l| l == "parent_mem=ok") {
            return Err(format!(
                "parent process is still dumpable ({why}) and the self-test did not prove \
                 the child is refused its memory"
            ));
        }
        warnings.push(format!(
            "parent process is dumpable ({why}); accepted for this process, and the child is \
             refused the parent's memory"
        ));
    }
    if let Some(reason) = seccomp_unavailable {
        if cfg!(target_os = "linux") && policy == SeccompPolicy::Required {
            return Err(format!(
                "seccomp is unavailable here ({reason}); set {SANDBOX_SECCOMP_ENV}=auto to run \
                 scenes in a separate process without it"
            ));
        }
        warnings.push(format!(
            "no seccomp ({reason}); scenes run in a separate process with an empty \
             environment{} only",
            if cfg!(target_os = "linux") {
                ", RLIMIT_DATA, RLIMIT_NPROC=0, RLIMIT_NOFILE at its open descriptors and \
                 parent-death kill"
            } else {
                ": an escaped scene would have this user's file, network and exec access"
            }
        ));
    }
    Ok(warnings)
}

/// The self-test plus one trivial scene through the full job path. Backs
/// `--scene-sandbox-check`, so a packaged binary proves it can run scenes where it is
/// deployed. Returns the probe report.
pub fn check() -> Result<String> {
    let report = self_test()?;
    let job = SceneJob {
        code: "__abgen.sendToRenderer(new Uint8Array([42]));".into(),
        main_crdt: None,
        read_file: Box::new(|name| bail!("check scene lists no files ({name})")),
        limits: EngineLimits {
            deadline: Some(Duration::from_secs(30)),
            ..EngineLimits::default()
        },
    };
    let out = SandboxedEngine.run_capture(job)?;
    if !out.sent || out.stream != [42] {
        bail!(
            "check scene produced sent={} stream={:?}, expected [42]",
            out.sent,
            out.stream
        );
    }
    Ok(report)
}

/// The command that starts a child: this binary, through the same loader and library
/// path when a launcher says so (`current_exe()` under an explicit `ld.so` is the loader).
fn child_command() -> Result<Command> {
    let bin = match std::env::var_os(SANDBOX_BIN_ENV) {
        Some(bin) => PathBuf::from(bin),
        None => std::env::current_exe().context("locate own binary for the scene sandbox")?,
    };
    let mut cmd = match std::env::var_os(SANDBOX_LOADER_ENV) {
        Some(loader) => {
            let mut cmd = Command::new(loader);
            if let Some(libpath) = std::env::var_os(SANDBOX_LIBPATH_ENV) {
                cmd.arg("--library-path").arg(libpath);
            }
            cmd.arg(&bin);
            cmd
        }
        None => Command::new(&bin),
    };
    cmd.arg(CHILD_ARG).arg(seccomp_policy().child_arg());
    Ok(cmd)
}

fn run_in_child(request: Request) -> Result<Response> {
    let mut cmd = child_command()?;
    harden_parent();

    cmd.env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe libc calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                linux::child_pre_exec();
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().context("spawn scene sandbox child")?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");

    let kill_after = match &request {
        Request::Job(job) => job
            .limits
            .deadline
            .map(|d| d + KILL_GRACE)
            .unwrap_or(KILL_WITHOUT_DEADLINE),
        Request::SelfTest => KILL_GRACE,
    };
    let child = Arc::new(Mutex::new(child));
    let watchdog = Watchdog::start(child.clone(), kill_after);

    let result = converse(request, stdin, stdout);
    // The pipes are closed now. A child that is still running after a protocol error
    // would otherwise be waited on with nothing but the watchdog to end it.
    let mut killed_on_error = false;
    if result.is_err() {
        let mut guard = lock(&child);
        if matches!(guard.try_wait(), Ok(None)) {
            killed_on_error = kill_child(&mut guard);
        }
    }
    // The watchdog stays armed until the child is gone, and it kills through the same
    // mutex, so this must not sit in `wait()` holding the lock: poll instead.
    let status = loop {
        let polled = lock(&child)
            .try_wait()
            .context("wait for scene sandbox child")?;
        if let Some(status) = polled {
            break status;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let timed_out = watchdog.stop();

    match result {
        // A complete answer stands even if the watchdog caught the child on its way out.
        Ok(response) => {
            if !status.success() {
                tracing::warn!("scene sandbox child exited with {status} after answering");
            }
            Ok(response)
        }
        Err(_) if timed_out => bail!(
            "scene sandbox child killed after {}s (scene runtime exceeded the {} deadline)",
            kill_after.as_secs(),
            crate::lodgen::simplify::SUBPROC_TIMEOUT_ENV
        ),
        Err(e) if killed_on_error || status.success() => Err(e),
        Err(e) => Err(e.context(format!(
            "scene sandbox child exited with {status} (the scene crashed the runtime or hit the sandbox)"
        ))),
    }
}

fn converse(
    request: Request,
    stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
) -> Result<Response> {
    let mut to_child = BufWriter::new(stdin);
    let mut from_child = BufReader::new(stdout);

    let (tag, hello) = read_frame(&mut from_child, MAX_CHILD_TEXT).context(
        "scene sandbox child did not start; does this binary call \
         scenerun::run_sandbox_child_if_requested() at the top of main?",
    )?;
    match tag {
        T_READY if hello == MAGIC => {}
        T_FAILED => bail!("scene sandbox child failed to start: {}", text(&hello)),
        _ => bail!("scene sandbox child sent an unexpected greeting (tag {tag})"),
    }

    let job = match request {
        Request::SelfTest => {
            write_frame(&mut to_child, T_SELF_TEST, &[])?;
            let (tag, body) = read_frame(&mut from_child, MAX_CHILD_TEXT)?;
            return match tag {
                T_SELF_TEST_REPORT => Ok(Response::SelfTest(text(&body))),
                T_FAILED => bail!("scene sandbox self-test failed: {}", text(&body)),
                _ => bail!("scene sandbox child sent tag {tag} to a self-test"),
            };
        }
        Request::Job(job) => job,
    };

    let SceneJob {
        code,
        main_crdt,
        read_file,
        limits,
    } = job;
    let job_frame = encode_job(&code, main_crdt.as_deref(), &limits, current_log_level());
    if job_frame.len() > MAX_PARENT_FRAME {
        bail!(
            "scene code and main.crdt total {} bytes, over the {MAX_PARENT_FRAME}-byte sandbox limit",
            job_frame.len()
        );
    }
    write_frame(&mut to_child, T_JOB, &job_frame)?;

    // Each tag gets its own size cap, applied before the body is allocated: a file name is
    // never 256 MiB.
    let frame_limit = |tag: u8| match tag {
        T_DONE => MAX_CHILD_FRAME,
        T_READ_FILE => MAX_FILE_NAME,
        T_LOG => MAX_CHILD_TEXT + 1,
        _ => MAX_CHILD_TEXT,
    };
    let mut reads = 0usize;
    let mut logs = 0usize;
    loop {
        let (tag, body) = read_frame_with(&mut from_child, frame_limit)?;
        match tag {
            T_LOG => {
                logs += 1;
                if logs <= MAX_LOG_LINES {
                    if let Some((level, msg)) = body.split_first() {
                        emit_child_log(*level, &text(msg));
                    }
                }
            }
            T_READ_FILE => {
                reads += 1;
                if reads > MAX_FILE_READS {
                    bail!("scene read more than {MAX_FILE_READS} files");
                }
                let name = std::str::from_utf8(&body)
                    .map_err(|_| anyhow!("scene asked for a non-UTF-8 file name"))?;
                match read_file(name) {
                    Ok((bytes, hash)) if 4 + hash.len() + bytes.len() > MAX_PARENT_FRAME => {
                        let msg = format!(
                            "file {name} is {} bytes, over the {MAX_PARENT_FRAME}-byte sandbox limit",
                            bytes.len()
                        );
                        write_frame(&mut to_child, T_FILE_ERR, msg.as_bytes())?;
                    }
                    Ok((bytes, hash)) => {
                        let mut out = Vec::with_capacity(4 + hash.len() + bytes.len());
                        put_bytes(&mut out, hash.as_bytes());
                        out.extend_from_slice(&bytes);
                        write_frame(&mut to_child, T_FILE_OK, &out)?;
                    }
                    Err(e) => write_frame(&mut to_child, T_FILE_ERR, format!("{e:#}").as_bytes())?,
                }
            }
            T_DONE => return decode_outcome(&body).map(Response::Capture),
            T_FAILED => bail!("{}", text(&body)),
            _ => bail!("scene sandbox child sent unexpected tag {tag}"),
        }
    }
}

/// Kills the child when the deadline passes; [`Watchdog::stop`] reports whether it did.
struct Watchdog {
    stop: mpsc::Sender<()>,
    handle: std::thread::JoinHandle<bool>,
}

impl Watchdog {
    fn start(child: Arc<Mutex<Child>>, after: Duration) -> Self {
        let (stop, stopped) = mpsc::channel::<()>();
        let handle = std::thread::Builder::new()
            .name("abgen-scenerun-watchdog".into())
            .spawn(move || match stopped.recv_timeout(after) {
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    kill_child(&mut lock(&child));
                    true
                }
                _ => false,
            })
            .expect("spawn scene sandbox watchdog");
        Watchdog { stop, handle }
    }

    fn stop(self) -> bool {
        let _ = self.stop.send(());
        self.handle.join().unwrap_or(false)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// Log levels on the wire: 0 = off, 1 = error … 5 = trace.
const LOG_OFF: u8 = 0;

fn level_to_wire(level: tracing::Level) -> u8 {
    use tracing::Level;
    if level == Level::ERROR {
        1
    } else if level == Level::WARN {
        2
    } else if level == Level::INFO {
        3
    } else if level == Level::DEBUG {
        4
    } else {
        5
    }
}

fn level_from_wire(level: u8) -> Option<tracing::Level> {
    Some(match level {
        1 => tracing::Level::ERROR,
        2 => tracing::Level::WARN,
        3 => tracing::Level::INFO,
        4 => tracing::Level::DEBUG,
        5 => tracing::Level::TRACE,
        _ => return None,
    })
}

/// The most verbose level the parent's subscriber would accept; the child forwards
/// nothing more verbose than this.
fn current_log_level() -> u8 {
    tracing::level_filters::LevelFilter::current()
        .into_level()
        .map_or(LOG_OFF, level_to_wire)
}

/// Re-emits a child event here, under this module's target, at its original level.
fn emit_child_log(level: u8, msg: &str) {
    match level {
        1 => tracing::error!("scene sandbox: {msg}"),
        2 => tracing::warn!("scene sandbox: {msg}"),
        3 => tracing::info!("scene sandbox: {msg}"),
        4 => tracing::debug!("scene sandbox: {msg}"),
        5 => tracing::trace!("scene sandbox: {msg}"),
        _ => {}
    }
}

/// Kills a child that is still running (or not yet reaped): on Linux its whole process
/// group, which it leads, so anything it forked without a filter goes with it. The
/// group id is the child's pid, which stays reserved until the child is reaped, so the
/// group kill cannot hit an unrelated process.
fn kill_child(child: &mut Child) -> bool {
    #[cfg(target_os = "linux")]
    linux::kill_group(child.id());
    child.kill().is_ok()
}

static DUMPABLE_PARENT_ACCEPTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Lets scenes run under a parent that stays dumpable (see the module docs). Only for a
/// process with no credentials or data the scene must not reach; call it before the first
/// scene. The child must still be refused access to the parent's memory.
pub fn accept_dumpable_parent() {
    DUMPABLE_PARENT_ACCEPTED.store(true, Ordering::Relaxed);
}

fn dumpable_parent_accepted() -> bool {
    DUMPABLE_PARENT_ACCEPTED.load(Ordering::Relaxed)
}

/// `PR_SET_DUMPABLE` failures, kept for the self-test's verdict.
#[cfg(target_os = "linux")]
static PARENT_DUMPABLE_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// Marks the parent non-dumpable (Linux). Whether that took, and whether the child has
/// seccomp, is judged by the self-test ([`evaluate_report`]), which also logs the warnings.
fn harden_parent() {
    #[cfg(target_os = "linux")]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            if let Err(e) = linux::make_non_dumpable() {
                *lock(&PARENT_DUMPABLE_ERROR) = Some(format!("PR_SET_DUMPABLE: {e}"));
            }
        });
    }
}

/// `None` when the parent is non-dumpable (always, off Linux); otherwise why it is not.
fn parent_dumpable_error() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        if linux::is_non_dumpable() {
            return None;
        }
        return Some(
            lock(&PARENT_DUMPABLE_ERROR)
                .clone()
                .unwrap_or_else(|| "PR_GET_DUMPABLE reports dumpable".into()),
        );
    }
    #[cfg(not(target_os = "linux"))]
    None
}

// ---------------------------------------------------------------------------------------
// Child side
// ---------------------------------------------------------------------------------------

/// If this process was started as a scene sandbox child, runs the child and exits;
/// otherwise returns immediately.
pub fn run_child_if_requested() {
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new(CHILD_ARG)) {
        return;
    }
    // The scene thread gets the same stack the in-process pool used. It is spawned before
    // the seccomp filter goes on (clone is not allowed afterwards).
    let worker = std::thread::Builder::new()
        .name("abgen-scenerun-sandbox".into())
        .stack_size(super::driver::SCENE_THREAD_STACK)
        .spawn(child_main);
    let code = match worker.map(|h| h.join()) {
        Ok(Ok(Ok(()))) => 0,
        Ok(Ok(Err(e))) => {
            eprintln!("scene sandbox child: {e:#}");
            1
        }
        Ok(Err(_)) => 101,
        Err(e) => {
            eprintln!("scene sandbox child: spawn worker: {e}");
            1
        }
    };
    std::process::exit(code);
}

type ChildIo = (
    BufReader<Box<dyn Read + Send>>,
    BufWriter<Box<dyn Write + Send>>,
);

fn child_main() -> Result<()> {
    let (input, output) = child_stdio();
    let io = Arc::new(Mutex::new((input, output)));

    let policy = child_seccomp_policy();
    let seccomp = match install_child_sandbox() {
        Ok(state) => state,
        Err(e) => {
            let mut guard = lock(&io);
            let _ = write_frame(&mut guard.1, T_FAILED, format!("{e:#}").as_bytes());
            return Err(e);
        }
    };
    {
        let mut guard = lock(&io);
        write_frame(&mut guard.1, T_READY, MAGIC)?;
    }

    let (tag, body) = {
        let mut guard = lock(&io);
        read_frame(&mut guard.0, MAX_PARENT_FRAME)?
    };
    match tag {
        T_SELF_TEST => {
            let report = probe_confinement(&seccomp);
            let mut guard = lock(&io);
            write_frame(&mut guard.1, T_SELF_TEST_REPORT, report.as_bytes())
        }
        // The parent never sends a job before a passing self-test; this is the child's
        // own refusal in case one arrives anyway.
        T_JOB if policy == SeccompPolicy::Required && cfg!(target_os = "linux") => {
            if let SeccompState::Unavailable(reason) = &seccomp {
                let msg = format!(
                    "scene refused: no seccomp filter ({reason}) and the policy is required"
                );
                let mut guard = lock(&io);
                write_frame(&mut guard.1, T_FAILED, bounded(&msg).as_bytes())?;
                bail!("{msg}");
            }
            run_child_job(&io, &body)
        }
        T_JOB => run_child_job(&io, &body),
        other => bail!("parent sent unexpected tag {other}"),
    }
}

fn run_child_job(io: &Arc<Mutex<ChildIo>>, body: &[u8]) -> Result<()> {
    let (code, main_crdt, limits, log_level) = decode_job(body)?;
    if let Some(max) = level_from_wire(log_level) {
        // Only fails if a subscriber is already set, which the child never does.
        let _ = tracing::subscriber::set_global_default(PipeLogger {
            io: io.clone(),
            max,
            sent: AtomicUsize::new(0),
        });
    }
    let read_io = io.clone();
    let read_file: super::ReadFileFn = Box::new(move |name: &str| child_read_file(&read_io, name));
    let job = SceneJob {
        code,
        main_crdt,
        read_file,
        limits,
    };
    let result = super::engine::run_capture_here(job);
    let mut guard = lock(io);
    match result {
        Ok(outcome) => write_frame(&mut guard.1, T_DONE, &encode_outcome(&outcome)),
        Err(e) => write_frame(
            &mut guard.1,
            T_FAILED,
            bounded(&format!("{e:#}")).as_bytes(),
        ),
    }
}

/// Serves a scene `readFile` through the parent. A refused file is an ordinary error the
/// scene sees as an exception; a broken exchange ends the child, because after it the
/// pipe is out of sync and the scene would otherwise run on unseen.
fn child_read_file(io: &Arc<Mutex<ChildIo>>, name: &str) -> Result<(Vec<u8>, String)> {
    let mut guard = lock(io);
    match read_file_exchange(&mut guard, name) {
        Ok(reply) => reply,
        Err(e) => {
            eprintln!("scene sandbox child: pipe out of sync, exiting: {e:#}");
            std::process::exit(3);
        }
    }
}

/// Outer error: the exchange itself broke. Inner error: the parent refused the file.
#[allow(clippy::type_complexity)]
fn read_file_exchange(io: &mut ChildIo, name: &str) -> Result<Result<(Vec<u8>, String)>> {
    write_frame(&mut io.1, T_READ_FILE, name.as_bytes())?;
    let (tag, body) = read_frame(&mut io.0, MAX_PARENT_FRAME)?;
    match tag {
        T_FILE_OK => {
            let mut cur = Cursor::new(&body);
            let hash = String::from_utf8(cur.bytes()?.to_vec())?;
            Ok(Ok((cur.rest().to_vec(), hash)))
        }
        T_FILE_ERR => Ok(Err(anyhow!("{}", text(&body)))),
        other => bail!("parent answered readFile with tag {other}"),
    }
}

#[cfg(unix)]
fn child_stdio() -> ChildIo {
    use std::os::fd::FromRawFd;
    // SAFETY: fds 0 and 1 are the pipes the parent set up and nothing else in the child
    // uses them; ownership moves into these Files for the rest of the process.
    unsafe {
        (
            BufReader::new(Box::new(std::fs::File::from_raw_fd(0))),
            BufWriter::new(Box::new(std::fs::File::from_raw_fd(1))),
        )
    }
}

#[cfg(windows)]
fn child_stdio() -> ChildIo {
    // The std handles stay owned by std; wrapping them in Files would close them twice.
    (
        BufReader::new(Box::new(std::io::stdin())),
        BufWriter::new(Box::new(std::io::stdout())),
    )
}

/// The child's tracing subscriber: forwards each event to the parent as a [`T_LOG`]
/// frame. Spans are not tracked; the engine and driver only emit events.
struct PipeLogger {
    io: Arc<Mutex<ChildIo>>,
    max: tracing::Level,
    sent: AtomicUsize,
}

impl tracing::Subscriber for PipeLogger {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= self.max
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::from_level(self.max))
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if self.sent.fetch_add(1, Ordering::Relaxed) >= MAX_LOG_LINES {
            return;
        }
        let mut msg = String::new();
        event.record(&mut MessageVisitor(&mut msg));
        let mut frame = Vec::with_capacity(1 + msg.len().min(MAX_CHILD_TEXT + 32));
        frame.push(level_to_wire(*event.metadata().level()));
        frame.extend_from_slice(bounded(&msg).as_bytes());
        // Never block on the pipe lock from inside an event: the holder is this same
        // thread in `child_read_file`, which emits nothing, so a miss is a bug guard.
        if let Ok(mut guard) = self.io.try_lock() {
            let _ = write_frame(&mut guard.1, T_LOG, &frame);
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Renders an event as `message field=value …`, the way the fmt subscriber would.
struct MessageVisitor<'a>(&'a mut String);

impl tracing::field::Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write;
        if field.name() == "message" {
            self.0.push_str(value);
        } else {
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }
}

/// The policy the parent passed as argv[2]. Absent or unknown reads as `required`: a child
/// started some other way must not run unconfined.
fn child_seccomp_policy() -> SeccompPolicy {
    child_seccomp_policy_from(std::env::args_os().nth(2).as_deref())
}

fn child_seccomp_policy_from(arg: Option<&OsStr>) -> SeccompPolicy {
    match arg {
        Some(arg) if arg == OsStr::new(CHILD_SECCOMP_AUTO_ARG) => SeccompPolicy::Auto,
        _ => SeccompPolicy::Required,
    }
}

/// Confines the child as far as this host allows. A runtime that refuses seccomp yields
/// `Unavailable` whatever the policy: the self-test reports it and the parent applies the
/// policy (`evaluate_report`), so under `required` the verdict is definitive and cached
/// rather than a start failure retried on every scene. The child itself still refuses a
/// job under `required` without the filter (`child_main`).
fn install_child_sandbox() -> Result<SeccompState> {
    let foreign = foreign_env_vars();
    if !foreign.is_empty() {
        bail!(
            "scene sandbox child started with a non-empty environment ({})",
            foreign.join(", ")
        );
    }
    #[cfg(target_os = "linux")]
    {
        // No further processes or threads from here on, whether or not the filter goes
        // on (which would deny clone anyway). Root is exempt from RLIMIT_NPROC.
        linux::forbid_new_processes();
        // No new descriptors either: only the pipes to the parent and stderr stay usable,
        // so without the filter the child still cannot open files or sockets.
        linux::forbid_new_files()?;
        match linux::install_seccomp() {
            Ok(()) => Ok(SeccompState::Installed),
            Err(linux::SeccompError::Refused(reason)) => Ok(SeccompState::Unavailable(reason)),
            Err(linux::SeccompError::Other(e)) => Err(e),
        }
    }
    #[cfg(not(target_os = "linux"))]
    Ok(SeccompState::Unavailable("no seccomp on this OS".into()))
}

/// Variables the platform itself puts into every process, even one started with an empty
/// `envp`. They carry nothing from the parent, so the empty-environment check ignores
/// them. On macOS, CoreFoundation sets `__CF_USER_TEXT_ENCODING` at load time when it is
/// missing, which any binary linking a system framework does.
#[cfg(target_os = "macos")]
const PLATFORM_INJECTED_ENV: &[&str] = &["__CF_USER_TEXT_ENCODING"];
#[cfg(not(target_os = "macos"))]
const PLATFORM_INJECTED_ENV: &[&str] = &[];

/// Names of environment variables the child has that the platform did not inject. Empty
/// when the parent's `env_clear()` took effect.
fn foreign_env_vars() -> Vec<String> {
    std::env::vars_os()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .filter(|name| !PLATFORM_INJECTED_ENV.contains(&name.as_str()))
        .collect()
}

/// Each probe must be refused by the sandbox; reported as `name=ok` or `name=FAIL(..)`.
/// The syscall probes run only with the filter on: without it they would all "fail", and
/// `seccomp=unavailable(..)` already says why.
fn probe_confinement(seccomp: &SeccompState) -> String {
    let foreign = foreign_env_vars();
    let empty_env = if foreign.is_empty() {
        "empty_env=ok".to_string()
    } else {
        format!("empty_env=FAIL(env present: {})", foreign.join(", "))
    };
    let mut lines = vec![empty_env];
    match seccomp {
        SeccompState::Installed => {
            lines.push("seccomp=ok".to_string());
            #[cfg(target_os = "linux")]
            lines.extend(linux::probes());
        }
        SeccompState::Unavailable(reason) => {
            // One line per probe; the parent strips only the final `)`, so parentheses
            // inside the reason are fine.
            lines.push(format!(
                "seccomp=unavailable({})",
                reason.replace('\n', " ")
            ));
            #[cfg(target_os = "linux")]
            lines.extend(linux::unfiltered_probes());
        }
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------------------
// Wire format: [tag u8][len u32 LE][payload]
// ---------------------------------------------------------------------------------------

fn write_frame(w: &mut impl Write, tag: u8, payload: &[u8]) -> Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| anyhow!("frame too large"))?;
    w.write_all(&[tag])?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

fn read_frame(r: &mut impl Read, max: usize) -> Result<(u8, Vec<u8>)> {
    read_frame_with(r, |_| max)
}

/// Reads one frame, sizing the limit by tag before the body is allocated.
fn read_frame_with(r: &mut impl Read, max_for: impl Fn(u8) -> usize) -> Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)
        .context("scene sandbox pipe closed")?;
    let len = u32::from_le_bytes(head[1..5].try_into().unwrap()) as usize;
    let max = max_for(head[0]);
    if len > max {
        bail!(
            "scene sandbox frame (tag {}) of {len} bytes exceeds the {max}-byte limit",
            head[0]
        );
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .context("scene sandbox pipe closed mid-frame")?;
    Ok((head[0], body))
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

struct Cursor<'a> {
    buf: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Cursor { buf }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() < n {
            bail!("truncated scene sandbox frame");
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize;
        self.take(n)
    }
    fn rest(self) -> &'a [u8] {
        self.buf
    }
}

const NO_DEADLINE: u64 = u64::MAX;

fn encode_job(
    code: &str,
    main_crdt: Option<&[u8]>,
    limits: &EngineLimits,
    log_level: u8,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + code.len() + main_crdt.map_or(0, <[u8]>::len));
    out.extend_from_slice(&(limits.memory_bytes as u64).to_le_bytes());
    out.extend_from_slice(&(limits.stack_bytes as u64).to_le_bytes());
    let deadline = limits.deadline.map_or(NO_DEADLINE, |d| {
        d.as_millis().min(u128::from(NO_DEADLINE - 1)) as u64
    });
    out.extend_from_slice(&deadline.to_le_bytes());
    out.extend_from_slice(&(limits.max_stream_bytes as u64).to_le_bytes());
    out.push(log_level);
    match main_crdt {
        Some(c) => {
            out.push(1);
            put_bytes(&mut out, c);
        }
        None => out.push(0),
    }
    out.extend_from_slice(code.as_bytes());
    out
}

type DecodedJob = (String, Option<Vec<u8>>, EngineLimits, u8);

fn decode_job(body: &[u8]) -> Result<DecodedJob> {
    let mut cur = Cursor::new(body);
    let memory_bytes = usize::try_from(cur.u64()?)?;
    let stack_bytes = usize::try_from(cur.u64()?)?;
    let deadline = match cur.u64()? {
        NO_DEADLINE => None,
        ms => Some(Duration::from_millis(ms)),
    };
    let max_stream_bytes = usize::try_from(cur.u64()?)?;
    let log_level = cur.u8()?;
    let main_crdt = match cur.u8()? {
        0 => None,
        1 => Some(cur.bytes()?.to_vec()),
        other => bail!("bad main.crdt flag {other}"),
    };
    let code = String::from_utf8(cur.rest().to_vec()).context("scene code is not UTF-8")?;
    Ok((
        code,
        main_crdt,
        EngineLimits {
            memory_bytes,
            stack_bytes,
            deadline,
            max_stream_bytes,
        },
        log_level,
    ))
}

fn encode_outcome(o: &CaptureOutcome) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + o.stream.len());
    out.push(u8::from(o.sent));
    match &o.eval_error {
        Some(e) => {
            out.push(1);
            put_bytes(&mut out, bounded(e).as_bytes());
        }
        None => out.push(0),
    }
    out.extend_from_slice(&o.stream);
    out
}

fn decode_outcome(body: &[u8]) -> Result<CaptureOutcome> {
    let mut cur = Cursor::new(body);
    let sent = match cur.u8()? {
        0 => false,
        1 => true,
        other => bail!("bad sent flag {other}"),
    };
    let eval_error = match cur.u8()? {
        0 => None,
        1 => {
            let e = cur.bytes()?;
            if e.len() > MAX_CHILD_TEXT {
                bail!("scene eval error of {} bytes", e.len());
            }
            Some(text(e))
        }
        other => bail!("bad eval-error flag {other}"),
    };
    Ok(CaptureOutcome {
        sent,
        stream: cur.rest().to_vec(),
        eval_error,
    })
}

/// Child-supplied text, lossily decoded and bounded.
fn text(b: &[u8]) -> String {
    bounded(&String::from_utf8_lossy(b))
}

/// Cuts `s` so the result, suffix included, is at most `MAX_CHILD_TEXT` bytes: what
/// `decode_outcome` and the log path accept.
fn bounded(s: &str) -> String {
    if s.len() <= MAX_CHILD_TEXT {
        return s.to_string();
    }
    let suffix = format!("… ({} bytes)", s.len());
    let mut end = MAX_CHILD_TEXT.saturating_sub(suffix.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &s[..end])
}

// ---------------------------------------------------------------------------------------
// Linux confinement
// ---------------------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux {
    use libc::{c_long, c_ulong, sock_filter, sock_fprog};

    pub(super) fn make_non_dumpable() -> std::io::Result<()> {
        // SAFETY: plain prctl on the current process.
        let rc = unsafe {
            libc::prctl(
                libc::PR_SET_DUMPABLE,
                0 as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// `SIGKILL` to the process group `pid` leads. Errors (no such group, already gone) are
    /// irrelevant to the caller, which kills the child itself next.
    pub(super) fn kill_group(pid: u32) {
        // SAFETY: plain signal to a group id derived from a live or unreaped child.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }

    /// `RLIMIT_NPROC = 0`: every later `fork`/`clone` by this user from this process fails
    /// with `EAGAIN` (root excepted). Called in the child after its worker thread exists.
    pub(super) fn forbid_new_processes() {
        let none = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: setrlimit on the current process with a valid struct.
        unsafe {
            libc::setrlimit(libc::RLIMIT_NPROC, &none);
        }
    }

    /// `RLIMIT_NOFILE` at the lowest descriptor number not in use (3: stdin, stdout,
    /// stderr), soft and hard: every later `open`, `socket`, `pipe` or `dup` fails with
    /// `EMFILE`, and an unprivileged process cannot raise it again.
    pub(super) fn forbid_new_files() -> anyhow::Result<()> {
        // SAFETY: fcntl on descriptor numbers only inspects them.
        let next_free = (0..1024)
            .find(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } == -1)
            .unwrap_or(1024) as libc::rlim_t;
        let limit = libc::rlimit {
            rlim_cur: next_free,
            rlim_max: next_free,
        };
        // SAFETY: setrlimit on the current process with a valid struct.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
            anyhow::bail!(
                "RLIMIT_NOFILE={next_free}: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    pub(super) fn is_non_dumpable() -> bool {
        // SAFETY: plain prctl on the current process.
        unsafe {
            libc::prctl(
                libc::PR_GET_DUMPABLE,
                0 as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
            ) == 0
        }
    }

    /// Runs in the forked child before exec; async-signal-safe calls only.
    pub(super) fn child_pre_exec() {
        // SAFETY: raw syscalls with constant arguments, no allocation.
        unsafe {
            libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
            );
            // Its own process group, so a kill reaches anything it manages to fork.
            libc::setpgid(0, 0);
            let core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &core);
            let data = libc::rlimit {
                rlim_cur: super::CHILD_DATA_LIMIT,
                rlim_max: super::CHILD_DATA_LIMIT,
            };
            libc::setrlimit(libc::RLIMIT_DATA, &data);
            // Mark everything but stdin/stdout/stderr close-on-exec, so the child starts
            // with only its pipes whether or not the parent's fds were CLOEXEC. Marking
            // rather than closing keeps std's own exec-status pipe usable: a failed exec
            // still reaches `spawn()` as an error instead of a silent "child did not start".
            if libc::syscall(
                libc::SYS_close_range,
                3 as c_ulong,
                !0u32 as c_ulong,
                CLOSE_RANGE_CLOEXEC,
            ) != 0
            {
                for fd in 3..4096 {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags >= 0 {
                        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                    }
                }
            }
        }
    }

    /// `close_range(2)` flag (Linux 5.11+); older kernels fail with EINVAL and take the
    /// `fcntl` loop above.
    const CLOSE_RANGE_CLOEXEC: c_ulong = 1 << 2;

    // Classic BPF opcodes and seccomp constants (linux/filter.h, linux/seccomp.h).
    const LD_W_ABS: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const JGE_K: u16 = 0x35;
    const JSET_K: u16 = 0x45;
    const RET_K: u16 = 0x06;
    const RET_KILL_PROCESS: u32 = 0x8000_0000;
    const RET_ERRNO: u32 = 0x0005_0000;
    const RET_ALLOW: u32 = 0x7fff_0000;
    const SET_MODE_FILTER: c_ulong = 1;
    const FLAG_TSYNC: c_ulong = 1;
    const OFF_NR: u32 = 0;
    const OFF_ARCH: u32 = 4;
    /// Low 32 bits of args[0].
    const OFF_ARG0: u32 = 16;
    /// Low 32 bits of args[2] (both supported targets are little-endian).
    const OFF_ARG2: u32 = 16 + 2 * 8;
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xC000_003E;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xC000_00B7;

    /// What a scene child needs once its thread exists and its pipes are open. The
    /// signal calls let a native fault die: Rust's SIGSEGV handler resets to `SIG_DFL`
    /// (`rt_sigaction`) and `abort()` raises SIGABRT (`getpid` + `tgkill`, the latter
    /// allowed only towards the child's own thread group in [`program`]); refusing them
    /// would leave a crashed child spinning until the watchdog.
    fn allowed() -> Vec<c_long> {
        let mut v = vec![
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_readv,
            libc::SYS_writev,
            libc::SYS_close,
            libc::SYS_munmap,
            libc::SYS_mremap,
            libc::SYS_madvise,
            libc::SYS_brk,
            libc::SYS_futex,
            libc::SYS_sched_yield,
            libc::SYS_clock_gettime,
            libc::SYS_clock_getres,
            libc::SYS_gettimeofday,
            libc::SYS_clock_nanosleep,
            libc::SYS_nanosleep,
            libc::SYS_getrandom,
            libc::SYS_rt_sigreturn,
            libc::SYS_rt_sigprocmask,
            libc::SYS_rt_sigaction,
            libc::SYS_sigaltstack,
            libc::SYS_gettid,
            libc::SYS_getpid,
            libc::SYS_exit,
            libc::SYS_exit_group,
        ];
        v.sort_unstable();
        v.dedup();
        v
    }

    fn stmt(code: u16, k: u32) -> sock_filter {
        sock_filter {
            code,
            jt: 0,
            jf: 0,
            k,
        }
    }

    fn jump(code: u16, k: u32, jt: u8, jf: u8) -> sock_filter {
        sock_filter { code, jt, jf, k }
    }

    fn program(own_pid: u32) -> Vec<sock_filter> {
        let deny = RET_ERRNO | libc::EPERM as u32;
        let mut p = vec![
            stmt(LD_W_ABS, OFF_ARCH),
            jump(JEQ_K, AUDIT_ARCH, 1, 0),
            stmt(RET_K, RET_KILL_PROCESS),
            stmt(LD_W_ABS, OFF_NR),
        ];
        #[cfg(target_arch = "x86_64")]
        {
            // x32 ABI syscall numbers alias the x86_64 table; never allow them.
            p.push(jump(JGE_K, 0x4000_0000, 0, 1));
            p.push(stmt(RET_K, RET_KILL_PROCESS));
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = JGE_K;
        for nr in allowed() {
            p.push(jump(JEQ_K, nr as u32, 0, 1));
            p.push(stmt(RET_K, RET_ALLOW));
        }
        // mmap/mprotect are allowed only without PROT_EXEC: no fresh executable memory.
        for nr in [libc::SYS_mmap, libc::SYS_mprotect] {
            p.push(jump(JEQ_K, nr as u32, 0, 4));
            p.push(stmt(LD_W_ABS, OFF_ARG2));
            p.push(jump(JSET_K, libc::PROT_EXEC as u32, 1, 0));
            p.push(stmt(RET_K, RET_ALLOW));
            p.push(stmt(RET_K, deny));
        }
        // tgkill only towards this process (args[0] is the tgid): enough for abort() and
        // raise(), never a signal to the parent or anything else running as this user.
        p.push(jump(JEQ_K, libc::SYS_tgkill as u32, 0, 4));
        p.push(stmt(LD_W_ABS, OFF_ARG0));
        p.push(jump(JEQ_K, own_pid, 0, 1));
        p.push(stmt(RET_K, RET_ALLOW));
        p.push(stmt(RET_K, deny));
        p.push(stmt(RET_K, deny));
        p
    }

    /// The parent's pid, taken before the filter goes on (`getppid` is not allowed after)
    /// so the self-test can prove a signal to it is refused.
    static PARENT_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

    /// Why the filter did not go on.
    pub(super) enum SeccompError {
        /// The runtime does not let this process use seccomp at all (`EPERM`, `EACCES`,
        /// `ENOSYS` from `prctl(PR_SET_NO_NEW_PRIVS)` or `seccomp(2)`): AWS Lambda does
        /// this. The policy decides whether to go on without it.
        Refused(String),
        /// Anything else, including a filter the kernel rejected: never tolerated.
        Other(anyhow::Error),
    }

    pub(super) fn classify(what: &str, err: std::io::Error) -> SeccompError {
        match err.raw_os_error() {
            Some(libc::EPERM) | Some(libc::EACCES) | Some(libc::ENOSYS) => {
                SeccompError::Refused(format!("{what}: {err}"))
            }
            _ => SeccompError::Other(anyhow::anyhow!("{what}: {err}")),
        }
    }

    /// Installs the filter on every thread of the child (TSYNC), after no_new_privs.
    pub(super) fn install_seccomp() -> std::result::Result<(), SeccompError> {
        // SAFETY: plain pid lookups on the current process.
        let (own_pid, parent_pid) = unsafe { (libc::getpid(), libc::getppid()) };
        PARENT_PID.store(parent_pid, std::sync::atomic::Ordering::Relaxed);
        let filter = program(own_pid as u32);
        let prog = sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_ptr() as *mut sock_filter,
        };
        // SAFETY: prctl/seccomp with a valid, live sock_fprog.
        unsafe {
            if libc::prctl(
                libc::PR_SET_NO_NEW_PRIVS,
                1 as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
            ) != 0
            {
                return Err(classify(
                    "PR_SET_NO_NEW_PRIVS",
                    std::io::Error::last_os_error(),
                ));
            }
            let rc = libc::syscall(
                libc::SYS_seccomp,
                SET_MODE_FILTER,
                FLAG_TSYNC,
                &prog as *const sock_fprog,
            );
            if rc != 0 {
                if rc > 0 {
                    return Err(SeccompError::Other(anyhow::anyhow!(
                        "seccomp TSYNC failed on thread {rc}"
                    )));
                }
                return Err(classify("seccomp", std::io::Error::last_os_error()));
            }
        }
        Ok(())
    }

    /// Runs one probe and reads errno right after it. `cleanup` releases whatever the
    /// probe got if it unexpectedly succeeded.
    fn refused(name: &str, probe: impl FnOnce() -> c_long, cleanup: impl FnOnce(c_long)) -> String {
        refused_with(name, libc::EPERM, probe, cleanup)
    }

    /// [`refused`] for a probe that must fail with `errno` rather than `EPERM`.
    fn refused_with(
        name: &str,
        errno: libc::c_int,
        probe: impl FnOnce() -> c_long,
        cleanup: impl FnOnce(c_long),
    ) -> String {
        let rc = probe();
        let err = std::io::Error::last_os_error().raw_os_error();
        if rc == -1 && err == Some(errno) {
            return format!("{name}=ok");
        }
        if rc >= 0 {
            cleanup(rc);
        }
        format!("{name}=FAIL(rc={rc} errno={err:?})")
    }

    /// Reads one word of the parent's memory with `process_vm_readv`. The kernel checks
    /// ptrace access before it looks at the address, so a refusal is `EPERM` and anything
    /// else (`EFAULT` on the arbitrary address included) means the access was allowed.
    fn read_parent_memory(parent_pid: libc::pid_t) -> c_long {
        let mut word = 0u64;
        let local = libc::iovec {
            iov_base: (&mut word as *mut u64).cast(),
            iov_len: std::mem::size_of::<u64>(),
        };
        let remote = libc::iovec {
            iov_base: 0x10000 as *mut libc::c_void,
            iov_len: std::mem::size_of::<u64>(),
        };
        // SAFETY: the local iovec points at a live u64; the remote one is only read by the
        // kernel on the parent's side, if at all.
        unsafe { libc::process_vm_readv(parent_pid, &local, 1, &remote, 1, 0) as c_long }
    }

    /// The child's limits without a filter: no new descriptors (`EMFILE`), no new
    /// processes (`EAGAIN` under `RLIMIT_NPROC=0`) and no access to the parent's memory.
    pub(super) fn unfiltered_probes() -> Vec<String> {
        // SAFETY: plain pid lookup.
        let parent_pid = unsafe { libc::getppid() };
        // SAFETY: as in `probes`: each probe fails (expected) or its result is released.
        unsafe {
            let close = |fd: c_long| {
                libc::close(fd as libc::c_int);
            };
            vec![
                refused_with(
                    "nofile_open",
                    libc::EMFILE,
                    || libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) as c_long,
                    close,
                ),
                refused_with(
                    "nofile_socket",
                    libc::EMFILE,
                    || libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) as c_long,
                    close,
                ),
                refused_with(
                    "nproc_clone",
                    libc::EAGAIN,
                    || {
                        let rc =
                            libc::syscall(libc::SYS_clone, libc::SIGCHLD as c_ulong, 0 as c_ulong);
                        if rc == 0 {
                            libc::_exit(0);
                        }
                        rc
                    },
                    |_| {},
                ),
                refused("parent_mem", || read_parent_memory(parent_pid), |_| {}),
            ]
        }
    }

    /// Syscalls outside the child's allowlist, each expected to fail with EPERM.
    pub(super) fn probes() -> Vec<String> {
        let parent_pid = PARENT_PID.load(std::sync::atomic::Ordering::Relaxed) as c_ulong;
        // SAFETY: each probe either fails (expected) or its result is released by the
        // matching cleanup; arguments are valid for the calls made. The tgkill probe sends
        // signal 0 (existence check only), so even unfiltered it would not harm the parent.
        unsafe {
            let close = |fd: c_long| {
                libc::close(fd as libc::c_int);
            };
            vec![
                refused(
                    "parent_mem",
                    || read_parent_memory(parent_pid as libc::pid_t),
                    |_| {},
                ),
                refused(
                    "tgkill_other",
                    || libc::syscall(libc::SYS_tgkill, parent_pid, parent_pid, 0 as c_ulong),
                    |_| {},
                ),
                refused(
                    "open",
                    || libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) as c_long,
                    close,
                ),
                refused(
                    "openat",
                    || {
                        libc::openat(libc::AT_FDCWD, c"/etc/hostname".as_ptr(), libc::O_RDONLY)
                            as c_long
                    },
                    close,
                ),
                refused(
                    "socket",
                    || libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) as c_long,
                    close,
                ),
                refused(
                    "mmap_exec",
                    || {
                        let p = libc::mmap(
                            std::ptr::null_mut(),
                            4096,
                            libc::PROT_READ | libc::PROT_EXEC,
                            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                            -1,
                            0,
                        );
                        if p == libc::MAP_FAILED {
                            -1
                        } else {
                            libc::munmap(p, 4096);
                            0
                        }
                    },
                    |_| {},
                ),
                refused(
                    "ptrace",
                    || {
                        libc::syscall(
                            libc::SYS_ptrace,
                            libc::PTRACE_TRACEME as c_ulong,
                            0 as c_ulong,
                            0 as c_ulong,
                            0 as c_ulong,
                        )
                    },
                    |_| {},
                ),
                refused(
                    "clone",
                    || {
                        let rc =
                            libc::syscall(libc::SYS_clone, libc::SIGCHLD as c_ulong, 0 as c_ulong);
                        if rc == 0 {
                            libc::_exit(0);
                        }
                        rc
                    },
                    |_| {},
                ),
                refused(
                    "execve",
                    || {
                        let path = c"/bin/true";
                        let argv = [path.as_ptr(), std::ptr::null()];
                        let envp = [std::ptr::null::<libc::c_char>()];
                        libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr()) as c_long
                    },
                    |_| {},
                ),
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_round_trips() {
        let limits = EngineLimits {
            memory_bytes: 123,
            stack_bytes: 456,
            deadline: Some(Duration::from_millis(789)),
            max_stream_bytes: 1011,
        };
        let body = encode_job("code ✓", Some(&[1, 2, 3]), &limits, 2);
        let (code, crdt, got, level) = decode_job(&body).unwrap();
        assert_eq!(code, "code ✓");
        assert_eq!(crdt.as_deref(), Some(&[1u8, 2, 3][..]));
        assert_eq!((got.memory_bytes, got.stack_bytes), (123, 456));
        assert_eq!(got.deadline, Some(Duration::from_millis(789)));
        assert_eq!(got.max_stream_bytes, 1011);
        assert_eq!(level_from_wire(level), Some(tracing::Level::WARN));

        let none = EngineLimits {
            deadline: None,
            ..limits
        };
        let (_, crdt, got, level) = decode_job(&encode_job("", None, &none, LOG_OFF)).unwrap();
        assert!(crdt.is_none());
        assert!(got.deadline.is_none());
        assert!(level_from_wire(level).is_none());
    }

    #[test]
    fn seccomp_policy_parses() {
        assert_eq!(SeccompPolicy::parse("auto"), Some(SeccompPolicy::Auto));
        assert_eq!(SeccompPolicy::parse(" AUTO "), Some(SeccompPolicy::Auto));
        assert_eq!(
            SeccompPolicy::parse("required"),
            Some(SeccompPolicy::Required)
        );
        assert_eq!(SeccompPolicy::parse(""), Some(SeccompPolicy::Required));
        assert_eq!(SeccompPolicy::parse("off"), None);
        assert_eq!(
            SeccompPolicy::Auto.child_arg(),
            CHILD_SECCOMP_AUTO_ARG,
            "the child reads this exact argv[2]"
        );
    }

    #[test]
    fn report_evaluation_follows_the_policy() {
        let confined = "empty_env=ok\nseccomp=ok\nparent_mem=ok\nopen=ok\nsocket=ok";
        assert_eq!(
            evaluate_report(confined, SeccompPolicy::Required, None, false),
            Ok(vec![])
        );
        assert_eq!(
            evaluate_report(confined, SeccompPolicy::Auto, None, false),
            Ok(vec![])
        );

        // A filter that is on but does not refuse is a failure under any policy.
        let leaky = "empty_env=ok\nseccomp=ok\nopen=FAIL(rc=3 errno=None)";
        for policy in [SeccompPolicy::Required, SeccompPolicy::Auto] {
            let err = evaluate_report(leaky, policy, None, false).unwrap_err();
            assert!(err.contains("open=FAIL"), "{err}");
        }

        let unavailable = "empty_env=ok\n\
             seccomp=unavailable(PR_SET_NO_NEW_PRIVS: Operation not permitted)\n\
             nofile_open=ok\nnofile_socket=ok\nnproc_clone=ok\nparent_mem=ok";
        let auto = evaluate_report(unavailable, SeccompPolicy::Auto, None, false).unwrap();
        assert_eq!(auto.len(), 1, "{auto:?}");
        assert!(auto[0].contains("PR_SET_NO_NEW_PRIVS"), "{auto:?}");
        let required = evaluate_report(unavailable, SeccompPolicy::Required, None, false);
        if cfg!(target_os = "linux") {
            let err = required.unwrap_err();
            assert!(err.contains(SANDBOX_SECCOMP_ENV), "{err}");
        } else {
            assert_eq!(required.unwrap().len(), 1);
        }

        // Without the filter, a descriptor or process limit that did not hold fails.
        let open_files = "empty_env=ok\nseccomp=unavailable(x)\n\
             nofile_open=FAIL(rc=3 errno=None)\nparent_mem=ok";
        let err = evaluate_report(open_files, SeccompPolicy::Auto, None, false).unwrap_err();
        assert!(err.contains("nofile_open=FAIL"), "{err}");

        // An environment the parent did not clear fails whatever the policy.
        let env = "empty_env=FAIL(env present: HOME)\nseccomp=unavailable(x)";
        assert!(evaluate_report(env, SeccompPolicy::Auto, None, false).is_err());

        // A dumpable parent is fatal under either policy, with the prctl error in the text.
        for policy in [SeccompPolicy::Required, SeccompPolicy::Auto] {
            let err = evaluate_report(confined, policy, Some("PR_SET_DUMPABLE: EPERM"), false)
                .unwrap_err();
            assert!(err.contains("dumpable") && err.contains("EPERM"), "{err}");
            let err = evaluate_report(unavailable, policy, Some("x"), false).unwrap_err();
            assert!(err.contains("dumpable"), "{err}");
        }
    }

    #[test]
    fn an_accepted_dumpable_parent_still_needs_its_memory_refused() {
        let unavailable = "empty_env=ok\nseccomp=unavailable(x)\n\
             nofile_open=ok\nnofile_socket=ok\nnproc_clone=ok\nparent_mem=ok";
        let warnings = evaluate_report(
            unavailable,
            SeccompPolicy::Auto,
            Some("PR_SET_DUMPABLE: EPERM"),
            true,
        )
        .unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("dumpable"), "{warnings:?}");

        // Memory access allowed: the probe line fails the report on its own.
        let readable = unavailable.replace("parent_mem=ok", "parent_mem=FAIL(rc=-1 errno=14)");
        let err = evaluate_report(&readable, SeccompPolicy::Auto, Some("x"), true).unwrap_err();
        assert!(err.contains("parent_mem=FAIL"), "{err}");

        // No memory probe in the report at all: not proven, so refused.
        let unproven = "empty_env=ok\nseccomp=unavailable(x)";
        let err = evaluate_report(unproven, SeccompPolicy::Auto, Some("x"), true).unwrap_err();
        assert!(err.contains("did not prove"), "{err}");
    }

    #[test]
    fn child_policy_defaults_to_required() {
        assert_eq!(child_seccomp_policy_from(None), SeccompPolicy::Required);
        assert_eq!(
            child_seccomp_policy_from(Some(OsStr::new("seccomp=off"))),
            SeccompPolicy::Required
        );
        assert_eq!(
            child_seccomp_policy_from(Some(OsStr::new(CHILD_SECCOMP_REQUIRED_ARG))),
            SeccompPolicy::Required
        );
        assert_eq!(
            child_seccomp_policy_from(Some(OsStr::new(CHILD_SECCOMP_AUTO_ARG))),
            SeccompPolicy::Auto
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_a_refusal_is_tolerated() {
        for errno in [libc::EPERM, libc::EACCES, libc::ENOSYS] {
            match linux::classify("seccomp", std::io::Error::from_raw_os_error(errno)) {
                linux::SeccompError::Refused(reason) => assert!(reason.starts_with("seccomp: ")),
                linux::SeccompError::Other(e) => panic!("errno {errno} classified as other: {e}"),
            }
        }
        for errno in [libc::EINVAL, libc::EFAULT, libc::ESRCH] {
            assert!(matches!(
                linux::classify("seccomp", std::io::Error::from_raw_os_error(errno)),
                linux::SeccompError::Other(_)
            ));
        }
    }

    #[test]
    fn platform_injected_env_is_not_foreign() {
        // The test process has an environment, so this only checks the filter itself.
        for name in PLATFORM_INJECTED_ENV {
            assert!(!foreign_env_vars().iter().any(|n| n == name));
        }
        std::env::set_var("ABGEN_SANDBOX_FOREIGN_PROBE", "1");
        assert!(foreign_env_vars()
            .iter()
            .any(|n| n == "ABGEN_SANDBOX_FOREIGN_PROBE"));
        std::env::remove_var("ABGEN_SANDBOX_FOREIGN_PROBE");
    }

    #[test]
    fn log_levels_round_trip() {
        for level in [
            tracing::Level::ERROR,
            tracing::Level::WARN,
            tracing::Level::INFO,
            tracing::Level::DEBUG,
            tracing::Level::TRACE,
        ] {
            assert_eq!(level_from_wire(level_to_wire(level)), Some(level));
        }
        assert!(level_from_wire(LOG_OFF).is_none());
        assert!(level_from_wire(6).is_none());
    }

    #[test]
    fn outcome_round_trips() {
        let o = CaptureOutcome {
            sent: true,
            stream: vec![9, 8, 7],
            eval_error: Some("boom".into()),
        };
        let got = decode_outcome(&encode_outcome(&o)).unwrap();
        assert!(got.sent);
        assert_eq!(got.stream, vec![9, 8, 7]);
        assert_eq!(got.eval_error.as_deref(), Some("boom"));
    }

    #[test]
    fn oversized_and_truncated_frames_are_rejected() {
        let mut buf = Vec::new();
        write_frame(&mut buf, T_DONE, &[0u8; 64]).unwrap();
        assert!(read_frame(&mut &buf[..], 63).is_err());
        assert!(read_frame(&mut &buf[..buf.len() - 1], 64).is_err());
        let (tag, body) = read_frame(&mut &buf[..], 64).unwrap();
        assert_eq!((tag, body.len()), (T_DONE, 64));
        assert!(decode_outcome(&[1]).is_err());
        assert!(decode_outcome(&[1, 1, 255, 255, 255, 255]).is_err());
    }

    #[test]
    fn child_text_is_bounded() {
        let long = "é".repeat(MAX_CHILD_TEXT);
        let got = bounded(&long);
        assert!(got.len() <= MAX_CHILD_TEXT, "{}", got.len());
        assert!(got.ends_with("bytes)"));
        // A bounded eval error always fits what the parent accepts.
        let o = CaptureOutcome {
            sent: false,
            stream: vec![],
            eval_error: Some(long),
        };
        let back = decode_outcome(&encode_outcome(&o)).unwrap();
        assert!(back.eval_error.unwrap().ends_with("bytes)"));
    }
}
