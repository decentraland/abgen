use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use abgen_core::export::{self, HostInfo, Kind, Sink};

const HOST: HostInfo = HostInfo::new("v-abgen-host", "host://out-of-process");

const FRAME_DONE: u32 = 0xFFFF_FFFF;

const DEFAULT_CAPPED_THREADS: usize = 8;

const EXIT_PROTOCOL: i32 = 64;
const EXIT_LIMIT: i32 = 65;
const EXIT_OUTPUT: i32 = 66;

const NAME_MAX_BYTES: usize = 240;

struct FrameSink {
    out: std::io::Stdout,
}

impl FrameSink {
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

struct DirSink {
    dir: PathBuf,
    inner: FrameSink,
    failed: AtomicBool,
}

impl DirSink {
    fn check_name(name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("refusing an artifact with an empty name".to_string());
        }
        if name.len() > NAME_MAX_BYTES {
            return Err(format!(
                "refusing artifact {name:?}: name is longer than {NAME_MAX_BYTES} bytes"
            ));
        }
        let mut parts = Path::new(name).components();
        let one_normal = matches!(parts.next(), Some(std::path::Component::Normal(_)));
        if !one_normal || parts.next().is_some() || name.contains('\0') {
            return Err(format!(
                "refusing artifact {name:?}: not a single path component"
            ));
        }
        #[cfg(windows)]
        {
            let stem = name.split('.').next().unwrap_or(name);
            let upper = stem.to_ascii_uppercase();
            let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                || matches!(
                    upper.as_bytes(),
                    [b'C', b'O', b'M', d] | [b'L', b'P', b'T', d]
                        if d.is_ascii_digit() && *d != b'0'
                );
            if reserved {
                return Err(format!(
                    "refusing artifact {name:?}: {stem} names a device on this platform"
                ));
            }
            if name.ends_with(' ') || name.ends_with('.') {
                return Err(format!(
                    "refusing artifact {name:?}: a trailing space or dot is not preserved here"
                ));
            }
        }
        Ok(())
    }

    fn write_artifact(&self, name: &str, data: &[u8]) -> Result<(), String> {
        Self::check_name(name)?;
        let tmp = self
            .dir
            .join(format!(".{name}.{}.part", std::process::id()));
        let describe = |what: &str, e: std::io::Error| format!("{what} {name:?}: {e}");

        let write = || -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(data)?;
            f.sync_all()
        };
        if let Err(e) = write() {
            let _ = std::fs::remove_file(&tmp);
            return Err(describe("could not write artifact", e));
        }
        if let Err(e) = std::fs::rename(&tmp, self.dir.join(name)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(describe("could not publish artifact", e));
        }
        Ok(())
    }
}

impl Sink for DirSink {
    fn emit_output(&self, name: &str, data: &[u8]) {
        match self.write_artifact(name, data) {
            Ok(()) => {
                let n = (name.len() as u32).to_le_bytes();
                let empty = 0u32.to_le_bytes();
                self.inner
                    .frame(Kind::Output as u32, &[&n, name.as_bytes(), &empty]);
            }
            Err(e) => {
                self.failed.store(true, Ordering::Relaxed);
                self.inner.emit(Kind::Error, e.as_bytes());
            }
        }
    }

    fn emit(&self, kind: Kind, bytes: &[u8]) {
        self.inner.emit(kind, bytes);
    }
}

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

#[cfg(target_os = "linux")]
const REEXEC_MARKER: &str = "ABGEN_HOST_MEMORY_LIMITED";

#[cfg(target_os = "linux")]
const REEXEC_LOADER: &str = "ABGEN_HOST_LOADER";
#[cfg(target_os = "linux")]
const REEXEC_LIBPATH: &str = "ABGEN_HOST_LIBPATH";
#[cfg(target_os = "linux")]
const REEXEC_BIN: &str = "ABGEN_HOST_BIN";

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

#[cfg(target_os = "macos")]
fn apply_memory_limit(_mb: u64) -> Result<(), String> {
    Err(
        "--max-memory-mb is unsupported on macOS: Darwin does not enforce \
         RLIMIT_AS/DATA/RSS (measured). Bound the work with --threads, or run \
         the helper under a sandbox profile or container that can cap memory"
            .to_string(),
    )
}

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
       --out-dir DIR       write artifacts into DIR and frame back their names\n  \
                           with empty payloads, instead of streaming the bytes.\n  \
                           Each is renamed into place, so anything present is\n  \
                           complete. DIR must already exist.\n  \
       --version           print the abgen version\n  \
       --help\n"
}

fn main() {
    let mut max_memory_mb: Option<u64> = None;
    let mut threads: Option<usize> = None;
    let mut out_dir: Option<PathBuf> = None;

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
            "--out-dir" => {
                let Some(d) = args.next() else {
                    eprintln!("abgen-host: --out-dir needs a directory");
                    std::process::exit(EXIT_PROTOCOL);
                };
                let path = PathBuf::from(d);
                if !path.is_dir() {
                    eprintln!("abgen-host: --out-dir {path:?} is not a directory");
                    std::process::exit(EXIT_PROTOCOL);
                }
                out_dir = Some(path);
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

    // Bounded by ABGEN_TEX_ENCODE_CACHE_MAX_MB / ABGEN_DECODE_CACHE_MB (see
    // crate/src/clihelp.rs), same as the lambda and live paths: a request
    // that spans several GLBs sharing textures (atlas reuse, wearable
    // collections) gets the dedup within this one process lifetime.
    abgen_core::texencode_cache::enable_with_profile(
        abgen_core::texencode_cache::CacheProfile::Client,
    );
    abgen_core::decode_cache::enable();

    let request = match read_request(&mut std::io::stdin()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("abgen-host: could not read the request: {e}");
            std::process::exit(EXIT_PROTOCOL);
        }
    };

    let frames = FrameSink {
        out: std::io::stdout(),
    };
    let (code, wrote_all) = match out_dir {
        None => (export::run(&request, &frames, HOST), true),
        Some(dir) => {
            let sink = DirSink {
                dir,
                inner: frames,
                failed: AtomicBool::new(false),
            };
            let code = export::run(&request, &sink, HOST);
            (code, !sink.failed.load(Ordering::Relaxed))
        }
    };
    let code = if wrote_all { code } else { EXIT_OUTPUT };

    let mut out = std::io::stdout().lock();
    let trailer = [FRAME_DONE.to_le_bytes(), (code as u32).to_le_bytes()].concat();
    if out.write_all(&trailer).is_err() || out.flush().is_err() {
        std::process::exit(EXIT_PROTOCOL);
    }
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_names_that_escape_the_directory() {
        for bad in [
            "../outside.bundle",
            "sub/dir.bundle",
            "..",
            ".",
            "",
            "/absolute.bundle",
        ] {
            assert!(
                DirSink::check_name(bad).is_err(),
                "should have refused {bad:?}"
            );
        }
        #[cfg(windows)]
        assert!(DirSink::check_name("sub\\dir.bundle").is_err());

        assert!(DirSink::check_name("entity_windows").is_ok());
        assert!(DirSink::check_name("a.b.c_mac.bundle").is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn refuses_windows_device_names_and_stripped_endings() {
        for bad in [
            "CON",
            "con",
            "Con",
            "NUL",
            "nul.bundle",
            "PRN",
            "AUX",
            "COM1",
            "com9",
            "LPT1",
            "lpt3.txt",
            "CON.bundle",
            "trailing.",
            "trailing ",
            "x...",
        ] {
            assert!(
                DirSink::check_name(bad).is_err(),
                "should have refused {bad:?}"
            );
        }
        for ok in [
            "COM0",
            "COM10",
            "CONSOLE",
            "console.bundle",
            "NULL",
            "context_windows",
        ] {
            assert!(
                DirSink::check_name(ok).is_ok(),
                "should have allowed {ok:?}"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn device_names_are_ordinary_files_off_windows() {
        for ok in ["CON", "nul.bundle", "COM1", "trailing.", "trailing "] {
            assert!(
                DirSink::check_name(ok).is_ok(),
                "should have allowed {ok:?}"
            );
        }
    }

    #[test]
    fn refuses_a_name_the_filesystem_would_reject() {
        let long = "x".repeat(NAME_MAX_BYTES + 1);
        let msg = DirSink::check_name(&long).unwrap_err();
        assert!(msg.contains("longer than"), "{msg}");
        assert!(DirSink::check_name(&"x".repeat(NAME_MAX_BYTES)).is_ok());
    }

    #[test]
    fn accepted_names_never_leave_the_directory() {
        let dir = std::env::temp_dir().join(format!("abgen_fuzz_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.canonicalize().unwrap();

        let mut cases: Vec<String> = vec![
            "..",
            ".",
            "",
            "/",
            "\\",
            "a/b",
            "a\\b",
            "../x",
            "..\\x",
            "/etc/passwd",
            "C:\\Windows\\x",
            "C:x",
            "\\\\server\\share\\x",
            "./x",
            ".../x",
            "x/..",
            "x/./y",
            "..%2fx",
            "%2e%2e/x",
            "\u{0}x",
            "x\u{0}",
            "x\u{0}/y",
            "CON",
            "PRN",
            "AUX",
            "NUL",
            "COM1",
            "LPT1",
            "con.bundle",
            "nul.txt",
            "x ",
            "x.",
            "x...",
            " x",
            ".hidden",
            ".x.1.part",
            "\u{202e}gnp.exe",
            "\u{feff}x",
            "x\u{a0}y",
            "\u{1f600}.bundle",
            "\r\n",
            "x\ty",
            "-",
            "--out-dir",
            "~",
            "$HOME",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let alphabet: Vec<char> = "ab/\\.:\u{0}\u{202e} .-~%".chars().collect();
        let mut state: u64 = 0x9e3779b97f4a7c15;
        for _ in 0..4000 {
            let mut s = String::new();
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (state >> 33) as usize % 12 + 1;
            for _ in 0..len {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                s.push(alphabet[(state >> 33) as usize % alphabet.len()]);
            }
            cases.push(s);
        }
        cases.push("x".repeat(NAME_MAX_BYTES + 1));
        cases.push("x".repeat(10_000));

        let sink = DirSink {
            dir: dir.clone(),
            inner: FrameSink {
                out: std::io::stdout(),
            },
            failed: AtomicBool::new(false),
        };

        let mut accepted = 0usize;
        for name in &cases {
            if DirSink::check_name(name).is_err() {
                continue;
            }
            accepted += 1;
            let target = dir.join(name);
            let parent = target.parent().unwrap();
            assert_eq!(
                parent
                    .canonicalize()
                    .unwrap_or_else(|_| parent.to_path_buf()),
                real,
                "accepted name escaped the directory: {name:?}"
            );
            if sink.write_artifact(name, b"z").is_ok() {
                let written = real.join(name);
                assert!(written.starts_with(&real), "wrote outside: {written:?}");
                assert!(written.is_file(), "not a regular file: {name:?}");
            }
        }
        assert!(
            accepted > 0,
            "fuzz corpus rejected everything - test is vacuous"
        );

        for entry in std::fs::read_dir(real.parent().unwrap()).unwrap() {
            let n = entry.unwrap().file_name();
            let n = n.to_string_lossy();
            assert!(
                !n.contains("abgen_fuzz_escape") && n != "z",
                "stray file beside the output dir: {n}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn publishes_atomically_and_leaves_no_temp_behind() {
        let dir = std::env::temp_dir().join(format!("abgen_dirsink_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let sink = DirSink {
            dir: dir.clone(),
            inner: FrameSink {
                out: std::io::stdout(),
            },
            failed: AtomicBool::new(false),
        };
        sink.write_artifact("scene_windows", b"bundle-bytes")
            .unwrap();

        assert_eq!(
            std::fs::read(dir.join("scene_windows")).unwrap(),
            b"bundle-bytes"
        );
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "unexpected leftovers: {left:?}");

        assert!(sink.write_artifact("../escape", b"x").is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
