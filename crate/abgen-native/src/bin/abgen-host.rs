//! Out-of-process conversion helper: the same [`abgen_core::export`] core as
//! the cdylib, behind a process boundary. One request per process, so a helper
//! that has already handled a hostile asset never handles a second.
//!
//! Protocol, little-endian throughout. stdin, once:
//!
//! ```text
//! u32 request_len | request bytes      (export::wire layout)
//! ```
//!
//! stdout, streamed, and carrying frames only — diagnostics go to stderr:
//!
//! ```text
//! u32 kind | u32 len | payload         repeated, kind = export::Kind
//! 0xFFFF_FFFF | u32 exit_code          exactly once, last
//! ```

use std::io::{Read, Write};

use abgen_core::export::{self, HostInfo, Kind, Sink};

const HOST: HostInfo = HostInfo::new("v-abgen-host", "host://out-of-process");

/// Trailer frame marker. Not a [`Kind`]; the only payload is the exit code.
const FRAME_DONE: u32 = 0xFFFF_FFFF;

const DEFAULT_CAPPED_THREADS: usize = 8;

const EXIT_PROTOCOL: i32 = 64;
const EXIT_LIMIT: i32 = 65;

/// Flushes every frame, so a streaming caller sees progress.
struct FrameSink {
    out: std::io::Stdout,
}

impl FrameSink {
    /// Never holds a second copy of the payload.
    fn frame(&self, kind: u32, parts: &[&[u8]]) {
        let total: usize = parts.iter().map(|p| p.len()).sum();
        let Ok(len) = u32::try_from(total) else {
            eprintln!("abgen-host: payload of {total} bytes exceeds u32");
            std::process::exit(EXIT_PROTOCOL);
        };
        let mut w = self.out.lock();
        let head = [kind.to_le_bytes(), len.to_le_bytes()].concat();
        if w.write_all(&head).is_err() {
            std::process::exit(EXIT_PROTOCOL);
        }
        for part in parts {
            if w.write_all(part).is_err() {
                std::process::exit(EXIT_PROTOCOL);
            }
        }
        let _ = w.flush();
    }
}

impl Sink for FrameSink {
    fn emit_output(&self, name: &str, data: &[u8]) {
        let n = (name.len() as u32).to_le_bytes();
        let d = (data.len() as u32).to_le_bytes();
        self.frame(Kind::Output as u32, &[&n, name.as_bytes(), &d, data]);
    }

    fn emit(&self, kind: Kind, bytes: &[u8]) {
        let Ok(len) = u32::try_from(bytes.len()) else {
            eprintln!("abgen-host: payload of {} bytes exceeds u32", bytes.len());
            std::process::exit(EXIT_PROTOCOL);
        };
        let mut w = self.out.lock();
        let head = [(kind as u32).to_le_bytes(), len.to_le_bytes()].concat();
        if w.write_all(&head).is_err() || w.write_all(bytes).is_err() {
            std::process::exit(EXIT_PROTOCOL);
        }
        let _ = w.flush();
    }
}

/// Refused before allocating: zero-filling 4 GiB on a bad four-byte prefix
/// aborts under a memory cap instead of erroring cleanly.
const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024 * 1024;

fn read_request(stdin: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stdin.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_REQUEST_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("request of {len} bytes exceeds the {MAX_REQUEST_BYTES}-byte maximum"),
        ));
    }
    let mut buf = vec![0u8; len];
    stdin.read_exact(&mut buf)?;
    Ok(buf)
}

/// Set once the limit is in force, so the re-executed process does not loop.
#[cfg(target_os = "linux")]
const REEXEC_MARKER: &str = "ABGEN_HOST_MEMORY_LIMITED";

/// How to re-exec when this binary is started through an explicit loader.
///
/// The release archive ships a private glibc and runs the helper as
/// `lib/ld.so --library-path lib bin/abgen-host.bin`, because the binary is
/// linked against a loader at an absolute path that exists only on the build
/// machine. That makes one archive work on NixOS and on ordinary distributions
/// alike, and it makes the host's glibc version irrelevant.
///
/// It also breaks the re-exec, which is why this exists. Under a loader the
/// kernel's executable is the loader, so `current_exe()` returns `ld.so` and
/// re-running it with our arguments gets `ld.so: unrecognized option
/// '--max-memory-mb'` — measured, exit 1, cap silently never applied. The
/// wrapper therefore states the real command instead of leaving us to infer
/// it: LOADER is the interpreter, LIBPATH its `--library-path`, BIN the actual
/// ELF. Unset — a plain `cargo build`, or a distribution that installed the
/// helper normally — and this falls back to `current_exe()`.
#[cfg(target_os = "linux")]
const REEXEC_LOADER: &str = "ABGEN_HOST_LOADER";
#[cfg(target_os = "linux")]
const REEXEC_LIBPATH: &str = "ABGEN_HOST_LIBPATH";
#[cfg(target_os = "linux")]
const REEXEC_BIN: &str = "ABGEN_HOST_BIN";

/// Applies `RLIMIT_AS` and re-executes.
///
/// The re-exec is the whole trick. By the time `main` runs mimalloc has
/// already reserved its arenas, so an in-process `setrlimit` barely binds —
/// measured, an 8 MB cap applied that way still converts a glb. Limits are
/// inherited across `exec`, so replacing the image gives an allocator that
/// initialises under the cap. Doing it here rather than in the parent is what
/// bounds children of callers that cannot set rlimits, Unity's
/// `Process.Start` most notably.
#[cfg(target_os = "linux")]
fn apply_memory_limit(mb: u64) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    if std::env::var_os(REEXEC_MARKER).is_some() {
        let mut cur = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: reading this process's own limit into a local.
        if unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut cur) } != 0 {
            return Err("could not read RLIMIT_AS back".to_string());
        }
        let want = mb.saturating_mul(1024 * 1024);
        if cur.rlim_cur > want {
            return Err(format!(
                "{REEXEC_MARKER} was set but RLIMIT_AS is {}, not {want}",
                cur.rlim_cur
            ));
        }
        return Ok(());
    }

    let bytes = mb.saturating_mul(1024 * 1024);
    let lim = libc::rlimit {
        rlim_cur: bytes,
        rlim_max: bytes,
    };
    // SAFETY: a well-formed rlimit for a resource this process owns.
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &lim) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }

    let mut cmd = match (
        std::env::var_os(REEXEC_LOADER),
        std::env::var_os(REEXEC_LIBPATH),
        std::env::var_os(REEXEC_BIN),
    ) {
        (Some(loader), Some(libpath), Some(bin)) => {
            let mut c = Command::new(loader);
            c.arg("--library-path").arg(libpath).arg(bin);
            c
        }
        (None, None, None) => {
            let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
            Command::new(exe)
        }
        _ => {
            return Err(format!(
                "{REEXEC_LOADER}/{REEXEC_LIBPATH}/{REEXEC_BIN} must be set together \
                 or not at all; re-executing with only some of them would run the \
                 wrong image under the memory cap"
            ))
        }
    };

    let err = cmd
        .args(std::env::args_os().skip(1))
        .env(REEXEC_MARKER, "1")
        .exec();
    Err(format!("re-exec under the memory limit failed: {err}"))
}

/// Darwin enforces no per-process memory rlimit: `setrlimit(RLIMIT_AS)`
/// returns `EINVAL` at every size, and measured on macOS 26 arm64,
/// `RLIMIT_AS`/`DATA`/`RSS` set from the parent at 256 MB all let a full
/// conversion finish. Refused loudly, because a cap that silently does
/// nothing is worse than none.
#[cfg(target_os = "macos")]
fn apply_memory_limit(_mb: u64) -> Result<(), String> {
    Err(
        "--max-memory-mb is unsupported on macOS: Darwin does not enforce \
         RLIMIT_AS/DATA/RSS (measured). Bound the work with --threads, or run \
         the helper under a sandbox profile or container that can cap memory"
            .to_string(),
    )
}

/// Applies a per-process commit cap via a job object.
///
/// No `exec` needed: `JOB_OBJECT_LIMIT_PROCESS_MEMORY` bounds *committed*
/// memory, and an allocator's up-front reservations are reserve-not-commit,
/// so a limit set here still binds on the allocations that matter. Nested
/// jobs (Windows 8+) let the process cap itself.
///
/// The scale differs sharply from the Linux arm for that same reason:
/// measured, this binds at 1-4 MB and passes from 8 MB up, where `RLIMIT_AS`
/// wants gigabytes. Exceeding it aborts the process rather than erroring,
/// which is what the boundary is for.
#[cfg(windows)]
fn apply_memory_limit(mb: u64) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let bytes = mb.saturating_mul(1024 * 1024) as usize;

    // SAFETY: documented Win32 calls, zeroed-then-populated struct of the size
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(format!(
                "CreateJobObject failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.ProcessMemoryLimit = bytes;

        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of!(info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            CloseHandle(job);
            return Err(format!("SetInformationJobObject failed: {e}"));
        }

        if AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
            let e = std::io::Error::last_os_error();
            CloseHandle(job);
            return Err(format!("AssignProcessToJobObject failed: {e}"));
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn apply_memory_limit(_mb: u64) -> Result<(), String> {
    Err("--max-memory-mb is not supported on this platform".to_string())
}

fn usage() -> &'static str {
    "abgen-host — out-of-process conversion helper\n\
     \n\
     Reads one length-prefixed request from stdin, streams framed results to\n\
     stdout, exits. Not meant to be run by hand; see crate/abgen-native.\n\
     \n\
     Options:\n  \
       --max-memory-mb N   cap memory (Linux RLIMIT_AS / Windows job object;\n  \
                           refused on macOS, which enforces neither).\n  \
                           Also bounds the worker pool unless --threads says\n  \
                           otherwise.\n  \
       --threads N         cap the CPU worker pool\n  \
       --version           print the abgen version\n  \
       --help\n"
}

fn main() {
    let mut max_memory_mb: Option<u64> = None;
    let mut threads: Option<usize> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--help" | "-h" => {
                print!("{}", usage());
                return;
            }
            "--version" | "-V" => {
                println!("{}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--max-memory-mb" => {
                max_memory_mb = args.next().and_then(|v| v.parse().ok());
                if max_memory_mb.is_none() {
                    eprintln!("abgen-host: --max-memory-mb needs a number");
                    std::process::exit(EXIT_PROTOCOL);
                }
            }
            "--threads" => {
                threads = args.next().and_then(|v| v.parse().ok());
                if threads.is_none() {
                    eprintln!("abgen-host: --threads needs a number");
                    std::process::exit(EXIT_PROTOCOL);
                }
            }
            other => {
                eprintln!("abgen-host: unknown argument {other:?}\n\n{}", usage());
                std::process::exit(EXIT_PROTOCOL);
            }
        }
    }

    if let Some(mb) = max_memory_mb {
        if let Err(e) = apply_memory_limit(mb) {
            eprintln!("abgen-host: could not apply memory limit: {e}");
            std::process::exit(EXIT_LIMIT);
        }
        if threads.is_none() {
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            threads = Some(cores.min(DEFAULT_CAPPED_THREADS));
        }
    }
    if let Some(n) = threads {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(n.max(1))
            .build_global();
    }

    let request = match read_request(&mut std::io::stdin()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("abgen-host: could not read the request: {e}");
            std::process::exit(EXIT_PROTOCOL);
        }
    };

    let sink = FrameSink {
        out: std::io::stdout(),
    };
    let code = export::run(&request, &sink, HOST);

    let mut out = std::io::stdout().lock();
    let trailer = [FRAME_DONE.to_le_bytes(), (code as u32).to_le_bytes()].concat();
    if out.write_all(&trailer).is_err() || out.flush().is_err() {
        std::process::exit(EXIT_PROTOCOL);
    }
    std::process::exit(code);
}
