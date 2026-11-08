//! Runs each scene script in its own short-lived child process.
//!
//! * The parent re-executes its own binary with [`CHILD_ARG`] and an empty environment,
//!   keeps only stdin/stdout/stderr open in the child, and has the child killed if the
//!   parent exits. The parent is marked non-dumpable.
//! * The child restricts itself with a seccomp allowlist before reading the job: I/O on
//!   the descriptors it already has, memory management without `PROT_EXEC`, futexes,
//!   clocks and exit. Other syscalls return `EPERM`.
//! * `readFile` is served by the parent over the pipe through the same content-listed
//!   closure as the in-process engine; the child does no network or filesystem access.
//!   The parent accepts only bounded, typed frames back.
//! * A deadline kills the child if the engine's own interrupt does not stop it.
//!
//! The first job in a process runs [`self_test`]; if the child setup is not in effect,
//! jobs fail instead of running. Non-Linux hosts get the separate process and empty
//! environment without seccomp, with a warning logged once.

use super::{CaptureOutcome, EngineLimits, SceneEngine, SceneJob};
use anyhow::{anyhow, bail, Context, Result};
use std::ffi::OsStr;
use std::io::{BufReader, BufWriter, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

/// argv[1] that turns any abgen binary into a scene sandbox child. Binaries that can run
/// scenes call [`run_child_if_requested`] first thing in `main`.
pub const CHILD_ARG: &str = "__abgen-scenerun-sandbox-child";

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

/// Bounds on what the parent accepts from the child.
const MAX_CHILD_FRAME: usize = 256 << 20;
const MAX_FILE_NAME: usize = 4096;
const MAX_CHILD_TEXT: usize = 4096;
const MAX_FILE_READS: usize = 10_000;
/// Bound on what the child accepts from the parent (scene code, main.crdt, files).
const MAX_PARENT_FRAME: usize = 1 << 30;

/// Slack over the engine's own deadline before the parent kills the child outright, and
/// the hard cap when the job has no deadline.
const KILL_GRACE: Duration = Duration::from_secs(30);
const KILL_WITHOUT_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// Runs each scene in a fresh sandboxed child process. See the module docs.
pub struct SandboxedEngine;

impl SceneEngine for SandboxedEngine {
    fn run_capture(&self, job: SceneJob) -> Result<CaptureOutcome> {
        ensure_sandbox_works()?;
        run_in_child(Request::Job(job))?.into_capture()
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

/// Spawns one child, runs the self-test in it and checks every probe was refused. Run once
/// per process before the first job; a broken sandbox fails every job instead of quietly
/// running scenes unconfined.
fn ensure_sandbox_works() -> Result<()> {
    static VERDICT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    VERDICT
        .get_or_init(|| self_test().map(drop).map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(|e| anyhow!("scene sandbox self-test failed, refusing to run scenes: {e}"))
}

/// Starts a child, has it probe its own confinement and returns its report
/// (`name=ok|FAIL(...)` per line). Errors if any probe was not refused.
pub fn self_test() -> Result<String> {
    let report = match run_in_child(Request::SelfTest)? {
        Response::SelfTest(r) => r,
        Response::Capture(_) => bail!("scene sandbox answered a self-test with a capture"),
    };
    let failed: Vec<&str> = report.lines().filter(|l| !l.ends_with("=ok")).collect();
    if !failed.is_empty() {
        bail!("sandbox probes not refused: {}", failed.join(", "));
    }
    if !parent_is_non_dumpable() {
        bail!("parent process is still dumpable");
    }
    Ok(report)
}

fn run_in_child(request: Request) -> Result<Response> {
    let exe = std::env::current_exe().context("locate own binary for the scene sandbox")?;
    harden_parent();

    let mut cmd = Command::new(exe);
    cmd.arg(CHILD_ARG)
        .env_clear()
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
    let timed_out = watchdog.stop();
    let status = lock(&child)
        .wait()
        .context("wait for scene sandbox child")?;

    if timed_out {
        bail!(
            "scene sandbox child killed after {}s (scene runtime exceeded the {} deadline)",
            kill_after.as_secs(),
            crate::lodgen::simplify::SUBPROC_TIMEOUT_ENV
        );
    }
    match result {
        Ok(response) if status.success() => Ok(response),
        Ok(_) => bail!("scene sandbox child exited with {status} after answering"),
        Err(e) if !status.success() => Err(e.context(format!(
            "scene sandbox child exited with {status} (the scene crashed the runtime or hit the sandbox)"
        ))),
        Err(e) => Err(e),
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
    write_frame(
        &mut to_child,
        T_JOB,
        &encode_job(&code, main_crdt.as_deref(), &limits),
    )?;

    let mut reads = 0usize;
    loop {
        let (tag, body) = read_frame(&mut from_child, MAX_CHILD_FRAME)?;
        match tag {
            T_READ_FILE => {
                reads += 1;
                if reads > MAX_FILE_READS {
                    bail!("scene read more than {MAX_FILE_READS} files");
                }
                if body.len() > MAX_FILE_NAME {
                    bail!("scene asked for a {}-byte file name", body.len());
                }
                let name = std::str::from_utf8(&body)
                    .map_err(|_| anyhow!("scene asked for a non-UTF-8 file name"))?;
                match read_file(name) {
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
                    let _ = lock(&child).kill();
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

fn harden_parent() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        #[cfg(target_os = "linux")]
        linux::make_non_dumpable();
        #[cfg(not(target_os = "linux"))]
        tracing::warn!(
            "scene sandbox: no seccomp on this OS; scenes run in a separate process with an \
             empty environment only"
        );
    });
}

fn parent_is_non_dumpable() -> bool {
    #[cfg(target_os = "linux")]
    return linux::is_non_dumpable();
    #[cfg(not(target_os = "linux"))]
    return true;
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

type ChildIo = (BufReader<std::fs::File>, BufWriter<std::fs::File>);

fn child_main() -> Result<()> {
    let (input, output) = child_stdio();
    let io = Arc::new(Mutex::new((input, output)));

    if let Err(e) = install_child_sandbox() {
        let mut guard = lock(&io);
        let _ = write_frame(&mut guard.1, T_FAILED, format!("{e:#}").as_bytes());
        return Err(e);
    }
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
            let report = probe_confinement();
            let mut guard = lock(&io);
            write_frame(&mut guard.1, T_SELF_TEST_REPORT, report.as_bytes())
        }
        T_JOB => {
            let (code, main_crdt, limits) = decode_job(&body)?;
            let read_io = io.clone();
            let read_file: super::ReadFileFn =
                Box::new(move |name: &str| child_read_file(&read_io, name));
            let job = SceneJob {
                code,
                main_crdt,
                read_file,
                limits,
            };
            let result = super::engine::run_capture_here(job);
            let mut guard = lock(&io);
            match result {
                Ok(outcome) => write_frame(&mut guard.1, T_DONE, &encode_outcome(&outcome)),
                Err(e) => write_frame(
                    &mut guard.1,
                    T_FAILED,
                    bounded(&format!("{e:#}")).as_bytes(),
                ),
            }
        }
        other => bail!("parent sent unexpected tag {other}"),
    }
}

fn child_read_file(io: &Arc<Mutex<ChildIo>>, name: &str) -> Result<(Vec<u8>, String)> {
    let mut guard = lock(io);
    write_frame(&mut guard.1, T_READ_FILE, name.as_bytes())?;
    let (tag, body) = read_frame(&mut guard.0, MAX_PARENT_FRAME)?;
    match tag {
        T_FILE_OK => {
            let mut cur = Cursor::new(&body);
            let hash = String::from_utf8(cur.bytes()?.to_vec())?;
            Ok((cur.rest().to_vec(), hash))
        }
        T_FILE_ERR => Err(anyhow!("{}", text(&body))),
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
            BufReader::new(std::fs::File::from_raw_fd(0)),
            BufWriter::new(std::fs::File::from_raw_fd(1)),
        )
    }
}

#[cfg(windows)]
fn child_stdio() -> ChildIo {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    // SAFETY: as above, with the process's standard handles.
    unsafe {
        (
            BufReader::new(std::fs::File::from_raw_handle(
                std::io::stdin().as_raw_handle(),
            )),
            BufWriter::new(std::fs::File::from_raw_handle(
                std::io::stdout().as_raw_handle(),
            )),
        )
    }
}

fn install_child_sandbox() -> Result<()> {
    if std::env::vars_os().next().is_some() {
        bail!("scene sandbox child started with a non-empty environment");
    }
    #[cfg(target_os = "linux")]
    linux::install_seccomp()?;
    Ok(())
}

/// Each probe must be refused by the sandbox; reported as `name=ok` or `name=FAIL(..)`.
fn probe_confinement() -> String {
    let mut lines = vec![format!(
        "empty_env={}",
        if std::env::vars_os().next().is_none() {
            "ok"
        } else {
            "FAIL(env present)"
        }
    )];
    #[cfg(target_os = "linux")]
    lines.extend(linux::probes());
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
    let mut head = [0u8; 5];
    r.read_exact(&mut head)
        .context("scene sandbox pipe closed")?;
    let len = u32::from_le_bytes(head[1..5].try_into().unwrap()) as usize;
    if len > max {
        bail!("scene sandbox frame of {len} bytes exceeds the {max}-byte limit");
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

fn encode_job(code: &str, main_crdt: Option<&[u8]>, limits: &EngineLimits) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + code.len() + main_crdt.map_or(0, <[u8]>::len));
    out.extend_from_slice(&(limits.memory_bytes as u64).to_le_bytes());
    out.extend_from_slice(&(limits.stack_bytes as u64).to_le_bytes());
    let deadline = limits.deadline.map_or(NO_DEADLINE, |d| {
        d.as_millis().min(u128::from(NO_DEADLINE - 1)) as u64
    });
    out.extend_from_slice(&deadline.to_le_bytes());
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

fn decode_job(body: &[u8]) -> Result<(String, Option<Vec<u8>>, EngineLimits)> {
    let mut cur = Cursor::new(body);
    let memory_bytes = usize::try_from(cur.u64()?)?;
    let stack_bytes = usize::try_from(cur.u64()?)?;
    let deadline = match cur.u64()? {
        NO_DEADLINE => None,
        ms => Some(Duration::from_millis(ms)),
    };
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
        },
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

fn bounded(s: &str) -> String {
    if s.len() <= MAX_CHILD_TEXT {
        return s.to_string();
    }
    let mut end = MAX_CHILD_TEXT;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", &s[..end], s.len())
}

// ---------------------------------------------------------------------------------------
// Linux confinement
// ---------------------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{bail, Result};
    use libc::{c_long, c_ulong, sock_filter, sock_fprog};

    pub(super) fn make_non_dumpable() {
        // SAFETY: plain prctl on the current process.
        unsafe {
            libc::prctl(
                libc::PR_SET_DUMPABLE,
                0 as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
                0 as c_ulong,
            );
        }
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
            let core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &core);
            // Close everything but stdin/stdout/stderr, whether or not it was CLOEXEC.
            if libc::syscall(
                libc::SYS_close_range,
                3 as c_ulong,
                !0u32 as c_ulong,
                0 as c_ulong,
            ) != 0
            {
                for fd in 3..4096 {
                    libc::close(fd);
                }
            }
        }
    }

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
    /// Low 32 bits of args[2] (both supported targets are little-endian).
    const OFF_ARG2: u32 = 16 + 2 * 8;
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xC000_003E;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xC000_00B7;

    /// What a scene child needs once its thread exists and its pipes are open.
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
            libc::SYS_sigaltstack,
            libc::SYS_gettid,
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

    fn program() -> Vec<sock_filter> {
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
        p.push(stmt(RET_K, deny));
        p
    }

    /// Installs the filter on every thread of the child (TSYNC), after no_new_privs.
    pub(super) fn install_seccomp() -> Result<()> {
        let filter = program();
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
                bail!("PR_SET_NO_NEW_PRIVS: {}", std::io::Error::last_os_error());
            }
            let rc = libc::syscall(
                libc::SYS_seccomp,
                SET_MODE_FILTER,
                FLAG_TSYNC,
                &prog as *const sock_fprog,
            );
            if rc != 0 {
                if rc > 0 {
                    bail!("seccomp TSYNC failed on thread {rc}");
                }
                bail!("seccomp: {}", std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Runs one probe and reads errno right after it. `cleanup` releases whatever the
    /// probe got if it unexpectedly succeeded.
    fn refused(name: &str, probe: impl FnOnce() -> c_long, cleanup: impl FnOnce(c_long)) -> String {
        let rc = probe();
        let err = std::io::Error::last_os_error().raw_os_error();
        if rc == -1 && err == Some(libc::EPERM) {
            return format!("{name}=ok");
        }
        if rc >= 0 {
            cleanup(rc);
        }
        format!("{name}=FAIL(rc={rc} errno={err:?})")
    }

    /// Syscalls outside the child's allowlist, each expected to fail with EPERM.
    pub(super) fn probes() -> Vec<String> {
        // SAFETY: each probe either fails (expected) or its result is released by the
        // matching cleanup; arguments are valid for the calls made.
        unsafe {
            let close = |fd: c_long| {
                libc::close(fd as libc::c_int);
            };
            vec![
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
        };
        let body = encode_job("code ✓", Some(&[1, 2, 3]), &limits);
        let (code, crdt, got) = decode_job(&body).unwrap();
        assert_eq!(code, "code ✓");
        assert_eq!(crdt.as_deref(), Some(&[1u8, 2, 3][..]));
        assert_eq!((got.memory_bytes, got.stack_bytes), (123, 456));
        assert_eq!(got.deadline, Some(Duration::from_millis(789)));

        let none = EngineLimits {
            deadline: None,
            ..limits
        };
        let (_, crdt, got) = decode_job(&encode_job("", None, &none)).unwrap();
        assert!(crdt.is_none());
        assert!(got.deadline.is_none());
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
        assert!(got.len() < MAX_CHILD_TEXT + 32);
        assert!(got.ends_with("bytes)"));
    }
}
