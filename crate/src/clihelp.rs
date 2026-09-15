//! Shared cache and concurrency defaults.
//!
//! Encode profiles are Lambda (4096 MiB memory, no disk), Client (256 MiB,
//! 2048 MiB disk), and Batch (4096 MiB, 8192 MiB disk).
//! `ABGEN_TEX_ENCODE_CACHE_MAX_MB`, `ABGEN_DISK_CACHE`,
//! `ABGEN_DISK_CACHE_DIR`, and `ABGEN_DISK_CACHE_MAX_MB` override them;
//! `ABGEN_DECODE_CACHE_MB` controls the 512 MiB decoded-image cache. Disk keys
//! include the crate version and `ABGEN_BUILD_ID`, and default off for dev builds.
//! File jobs use `min(cores, max(4, ram_gib / 4))`, overridden by
//! `ABGEN_FILE_CONCURRENCY`; network jobs use `(4 * cores).clamp(8, 32)`.

/// Free-to-use system memory in GiB, or `None` if it can't be determined
/// (unsupported platform, or the read failed). Cheap and dependency-free:
/// Linux parses `MemAvailable` out of `/proc/meminfo` (accounts for
/// reclaimable cache, so it is a real "free to use" figure). macOS calls
/// `sysctlbyname("hw.memsize")` via the `libc` dependency already in
/// `crate/Cargo.toml`, which is *total* physical memory — getting true
/// available memory needs the Mach `host_statistics64` API, not worth the
/// complexity for a coarse concurrency cap; total is a fine upper bound.
#[cfg(target_os = "linux")]
fn available_memory_gib() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = text
        .lines()
        .find(|l| l.starts_with("MemAvailable:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())?;
    Some(kb / (1024 * 1024))
}

#[cfg(target_os = "macos")]
fn available_memory_gib() -> Option<u64> {
    let mut mem: u64 = 0;
    let mut size: libc::size_t = std::mem::size_of::<u64>();
    // SAFETY: `hw.memsize` is a well-known macOS sysctl returning a u64;
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut mem as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && mem > 0).then_some(mem / (1024 * 1024 * 1024))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn available_memory_gib() -> Option<u64> {
    None
}

/// Default bounded file-level conversion concurrency: `min(cores, max(4,
/// ram_gib / 4))`. See the module doc for the rationale; `ABGEN_FILE_CONCURRENCY`
/// overrides this outright when set to a positive integer.
pub fn default_file_concurrency() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        if let Some(n) = std::env::var("ABGEN_FILE_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
        {
            return n;
        }
        match available_memory_gib() {
            Some(ram_gib) => cores.min(((ram_gib / 4) as usize).max(4)),
            None => cores,
        }
    })
}

/// Default bounded concurrency for network-bound work (content fetch, S3
/// probes): `(4 * cores).clamp(8, 32)`, uncoupled from the ~4 GiB/file RAM
/// budget behind [`default_file_concurrency`]. `ABGEN_FETCH_CONCURRENCY` /
/// `ABGEN_PROBE_CONCURRENCY` remain the per-call escape hatches.
pub fn default_network_concurrency() -> usize {
    static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        (cores * 4).clamp(8, 32)
    })
}

pub fn version_line(bin: &str) -> String {
    format!(
        "{bin} {} ({})",
        env!("CARGO_PKG_VERSION"),
        option_env!("ABGEN_BUILD_ID").unwrap_or("unknown")
    )
}

#[cfg(not(target_arch = "wasm32"))]
pub fn print_version(bin: &str) -> ! {
    println!("{}", version_line(bin));
    std::process::exit(0);
}

#[cfg(not(target_arch = "wasm32"))]
pub fn print_help(usage: &str) -> ! {
    println!("{}", usage.trim_end());
    std::process::exit(0);
}

#[cfg(not(target_arch = "wasm32"))]
pub fn usage_error(usage: &str) -> ! {
    eprintln!("{}", usage.trim_end());
    std::process::exit(2);
}

#[cfg(not(target_arch = "wasm32"))]
pub fn bad_flag(flag: &str, usage: &str) -> ! {
    eprintln!("unknown option: {flag}");
    usage_error(usage);
}

pub fn bool_token(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

pub fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Err(_) => default,
        Ok(v) => {
            let t = v.trim();
            if t.is_empty() {
                default
            } else {
                bool_token(t).unwrap_or_else(|| {
                    eprintln!(
                        "warning: {name}={t}: unrecognized boolean (use 1/true/yes/on or 0/false/no/off); keeping default {default}"
                    );
                    default
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_shape() {
        let v = version_line("abgen");
        assert!(v.starts_with("abgen "));
        assert!(v.contains(env!("CARGO_PKG_VERSION")));
        assert!(v.ends_with(')'));
    }

    #[test]
    fn default_file_concurrency_is_at_least_one() {
        assert!(default_file_concurrency() >= 1);
    }

    #[test]
    fn default_network_concurrency_is_clamped() {
        let n = default_network_concurrency();
        assert!((8..=32).contains(&n), "expected 8..=32, got {n}");
    }

    #[test]
    fn available_memory_gib_is_sane_when_detected() {
        if let Some(gib) = available_memory_gib() {
            assert!(gib >= 1, "detected {gib} GiB, suspiciously small");
        }
    }

    #[test]
    fn bool_tokens() {
        for t in ["1", "true", "YES", "On"] {
            assert_eq!(bool_token(t), Some(true));
        }
        for t in ["0", "false", "NO", "Off"] {
            assert_eq!(bool_token(t), Some(false));
        }
        assert_eq!(bool_token("maybe"), None);
        assert_eq!(bool_token(""), None);
    }

    #[test]
    fn env_bool_grammar() {
        let k = "ABGEN_TEST_ENV_BOOL_GRAMMAR";
        std::env::remove_var(k);
        assert!(!env_bool(k, false));
        assert!(env_bool(k, true));
        std::env::set_var(k, "1");
        assert!(env_bool(k, false));
        std::env::set_var(k, "off");
        assert!(!env_bool(k, true));
        std::env::set_var(k, "some-other-value");
        assert!(!env_bool(k, false));
        assert!(env_bool(k, true));
        std::env::set_var(k, "  ");
        assert!(!env_bool(k, false));
        assert!(env_bool(k, true));
        std::env::remove_var(k);
    }
}
