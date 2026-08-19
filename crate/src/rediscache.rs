//! Redis hit-cache in front of `space` existence probes (opt-in via
//! `ABGEN_REDIS_URL`; unset = no-op), mirroring consumer-server's asset-reuse
//! cache. S3 stays the source of truth: only S3-confirmed positives are
//! stored (canonical keys are immutable, so a positive can't go stale;
//! negatives never — a concurrent build may upload the object any moment),
//! and every Redis error fails open to the real probe with a backoff.
//!
//! `rediss://` speaks the same RESP over rustls, for clusters with in-transit
//! encryption required; server certificates are verified against the webpki
//! root bundle.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(1);
const FAILURE_BACKOFF: Duration = Duration::from_secs(30);
const DEFAULT_TTL_SECONDS: u64 = 86_400;

#[derive(Clone, Debug, PartialEq)]
struct Target {
    host: String,
    port: u16,
    user: Option<String>,
    password: Option<String>,
    db: Option<u32>,
    tls: bool,
}

fn parse_url(url: &str) -> Result<Target, String> {
    let (rest, tls) = match url.strip_prefix("rediss://") {
        Some(rest) => (rest, true),
        None => (
            url.strip_prefix("redis://")
                .ok_or_else(|| format!("unsupported scheme in {url:?} (expected redis(s)://)"))?,
            false,
        ),
    };
    let (rest, db) = match rest.split_once('/') {
        Some((r, d)) if !d.is_empty() => {
            let db = d
                .parse::<u32>()
                .map_err(|_| format!("bad db index {d:?}"))?;
            (r, Some(db))
        }
        Some((r, _)) => (r, None),
        None => (rest, None),
    };
    let (user, password, hostport) = match rest.rsplit_once('@') {
        Some((userinfo, hp)) => {
            let (user, pass) = match userinfo.split_once(':') {
                Some((u, p)) => (Some(u.to_string()).filter(|u| !u.is_empty()), p),
                None => (None, userinfo),
            };
            (user, Some(pass.to_string()).filter(|p| !p.is_empty()), hp)
        }
        None => (None, None, rest),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| format!("bad port {p:?}"))?),
        None => (hostport, 6379),
    };
    if host.is_empty() {
        return Err(format!("no host in {url:?}"));
    }
    Ok(Target {
        host: host.to_string(),
        port,
        user,
        password,
        db,
        tls,
    })
}

fn encode_command(args: &[&str]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

#[derive(Debug, PartialEq)]
enum Reply {
    Simple(String),
    Error(String),
    Integer(i64),
    Bulk(Option<Vec<u8>>),
}

fn io_err(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

fn read_line(r: &mut impl BufRead) -> std::io::Result<String> {
    let mut line = String::new();
    r.read_line(&mut line)?;
    if !line.ends_with("\r\n") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "truncated redis reply",
        ));
    }
    line.truncate(line.len() - 2);
    Ok(line)
}

fn read_reply(r: &mut impl BufRead) -> std::io::Result<Reply> {
    let line = read_line(r)?;
    let Some(kind) = line.chars().next() else {
        return Err(io_err("empty redis reply".to_string()));
    };
    let rest = &line[1..];
    match kind {
        '+' => Ok(Reply::Simple(rest.to_string())),
        '-' => Ok(Reply::Error(rest.to_string())),
        ':' => rest
            .parse::<i64>()
            .map(Reply::Integer)
            .map_err(|_| io_err(format!("bad integer reply {rest:?}"))),
        '$' => {
            let n = rest
                .parse::<i64>()
                .map_err(|_| io_err(format!("bad bulk length {rest:?}")))?;
            if n < 0 {
                return Ok(Reply::Bulk(None));
            }
            let mut buf = vec![0u8; n as usize + 2];
            r.read_exact(&mut buf)?;
            buf.truncate(n as usize);
            Ok(Reply::Bulk(Some(buf)))
        }
        other => Err(io_err(format!("unexpected redis reply type {other:?}"))),
    }
}

/// Plain TCP or the same socket wrapped in a rustls session (`rediss://`,
/// what ElastiCache calls in-transit encryption). Both are blocking, so the
/// RESP codec above is unaware of which one it is talking through.
enum Stream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Stream {
    fn socket(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => &s.sock,
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// Roots for `rediss://`, built once: the webpki bundle ureq already carries,
/// which chains ElastiCache's Amazon-issued server certificates.
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .expect("ring provider supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
            Arc::new(cfg)
        })
        .clone()
}

fn tls_handshake(host: &str, sock: TcpStream) -> std::io::Result<Stream> {
    let server = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| io_err(format!("bad TLS server name {host:?}: {e}")))?;
    let session = rustls::ClientConnection::new(tls_config(), server)
        .map_err(|e| io_err(format!("TLS setup failed: {e}")))?;
    let mut stream = rustls::StreamOwned::new(session, sock);
    // Drives the handshake to completion, so a bad peer/cert fails here rather
    // than inside the first command.
    stream.flush()?;
    Ok(Stream::Tls(Box::new(stream)))
}

struct Conn {
    io: BufReader<Stream>,
}

impl Conn {
    fn command(&mut self, args: &[&str]) -> std::io::Result<Reply> {
        let stream = self.io.get_mut();
        stream.write_all(&encode_command(args))?;
        stream.flush()?;
        read_reply(&mut self.io)
    }

    /// An `-ERR` reply becomes an io error so the caller tears the connection down.
    fn expect_ok(&mut self, args: &[&str]) -> std::io::Result<()> {
        match self.command(args)? {
            Reply::Error(e) => Err(io_err(format!("{}: {e}", args[0]))),
            _ => Ok(()),
        }
    }
}

fn connect(target: &Target) -> std::io::Result<Conn> {
    use std::net::ToSocketAddrs;
    let addr = (target.host.as_str(), target.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "redis host resolved to nothing",
            )
        })?;
    let sock = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    // The handshake costs round-trips a single command doesn't, so it gets the
    // connect budget; steady-state IO is tightened back down afterwards.
    sock.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    sock.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    sock.set_nodelay(true)?;
    let stream = if target.tls {
        tls_handshake(&target.host, sock)?
    } else {
        Stream::Plain(sock)
    };
    stream.socket().set_read_timeout(Some(IO_TIMEOUT))?;
    stream.socket().set_write_timeout(Some(IO_TIMEOUT))?;
    let mut conn = Conn {
        io: BufReader::new(stream),
    };
    if let Some(pass) = &target.password {
        // 2-arg AUTH when the URL names a user (Redis 6 ACLs / ElastiCache RBAC).
        match &target.user {
            Some(user) => conn.expect_ok(&["AUTH", user, pass])?,
            None => conn.expect_ok(&["AUTH", pass])?,
        }
    }
    if let Some(db) = target.db {
        conn.expect_ok(&["SELECT", &db.to_string()])?;
    }
    Ok(conn)
}

struct ConnSlot {
    conn: Option<Conn>,
    last_failure: Option<Instant>,
}

struct State {
    target: Target,
    ttl_seconds: u64,
    slot: Mutex<ConnSlot>,
}

fn state() -> Option<&'static State> {
    static S: OnceLock<Option<State>> = OnceLock::new();
    S.get_or_init(|| {
        let url = std::env::var("ABGEN_REDIS_URL")
            .ok()
            .filter(|s| !s.is_empty())?;
        match parse_url(&url) {
            Ok(target) => {
                let ttl_seconds = std::env::var("ABGEN_REDIS_TTL_SECONDS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .filter(|&t| t > 0)
                    .unwrap_or(DEFAULT_TTL_SECONDS);
                Some(State {
                    target,
                    ttl_seconds,
                    slot: Mutex::new(ConnSlot {
                        conn: None,
                        last_failure: None,
                    }),
                })
            }
            Err(e) => {
                tracing::warn!(error = %e, "ABGEN_REDIS_URL invalid; redis hit-cache disabled");
                None
            }
        }
    })
    .as_ref()
}

pub fn enabled() -> bool {
    state().is_some()
}

impl State {
    fn with_conn<T>(
        &self,
        op: &'static str,
        f: impl FnOnce(&mut Conn) -> std::io::Result<T>,
    ) -> Option<T> {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.conn.is_none() {
            if let Some(at) = slot.last_failure {
                if at.elapsed() < FAILURE_BACKOFF {
                    return None;
                }
            }
            match connect(&self.target) {
                Ok(c) => slot.conn = Some(c),
                Err(e) => {
                    slot.last_failure = Some(Instant::now());
                    metrics::counter!("abgen_rediscache_total", "op" => "connect", "result" => "error")
                        .increment(1);
                    tracing::warn!(
                        host = %self.target.host,
                        error = %e,
                        "redis connect failed; hit-cache bypassed for {FAILURE_BACKOFF:?}"
                    );
                    return None;
                }
            }
        }
        let conn = slot.conn.as_mut().expect("connection just ensured");
        match f(conn) {
            Ok(v) => {
                slot.last_failure = None;
                Some(v)
            }
            Err(e) => {
                slot.conn = None;
                slot.last_failure = Some(Instant::now());
                // Op errors count too — an outage must not look like normal
                // misses (upstream: ab_converter_redis_cache_errors_total).
                metrics::counter!("abgen_rediscache_total", "op" => op, "result" => "error")
                    .increment(1);
                tracing::warn!(error = %e, "redis {op} failed; hit-cache bypassed for {FAILURE_BACKOFF:?}");
                None
            }
        }
    }

    fn hit(&self, key: &str) -> bool {
        let r = self.with_conn("exists", |c| match c.command(&["EXISTS", key])? {
            Reply::Integer(n) => Ok(n > 0),
            Reply::Error(e) => Err(io_err(format!("EXISTS: {e}"))),
            other => Err(io_err(format!("EXISTS: unexpected reply {other:?}"))),
        });
        let result = match r {
            Some(true) => "hit",
            Some(false) => "miss",
            None => return false,
        };
        metrics::counter!("abgen_rediscache_total", "op" => "exists", "result" => result)
            .increment(1);
        r == Some(true)
    }

    fn mark(&self, key: &str) {
        let ttl = self.ttl_seconds.to_string();
        self.with_conn("set", |c| c.expect_ok(&["SET", key, "1", "EX", &ttl]));
    }

    fn forget(&self, key: &str) {
        self.with_conn("del", |c| match c.command(&["DEL", key])? {
            Reply::Error(e) => Err(io_err(format!("DEL: {e}"))),
            _ => Ok(()),
        });
    }
}

/// True iff the key is present. Any error or disabled cache is a miss.
pub fn hit(key: &str) -> bool {
    state().is_some_and(|st| st.hit(key))
}

/// Records a confirmed-positive existence check. Fire-and-forget.
pub fn mark(key: &str) {
    if let Some(st) = state() {
        st.mark(key);
    }
}

/// Drops a key so the next check goes back to the source of truth.
pub fn forget(key: &str) {
    if let Some(st) = state() {
        st.forget(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::TcpListener;

    #[test]
    fn parses_plain_host() {
        assert_eq!(
            parse_url("redis://cache.internal").unwrap(),
            Target {
                host: "cache.internal".to_string(),
                port: 6379,
                user: None,
                password: None,
                db: None,
                tls: false,
            }
        );
    }

    #[test]
    fn parses_tls_urls() {
        assert_eq!(
            parse_url("rediss://:sekret@cache.internal:6380/1").unwrap(),
            Target {
                host: "cache.internal".to_string(),
                port: 6380,
                user: None,
                password: Some("sekret".to_string()),
                db: Some(1),
                tls: true,
            }
        );
        // Same default port as plain redis — ElastiCache in-transit encryption
        // keeps 6379.
        let t = parse_url("rediss://cache.internal").unwrap();
        assert_eq!((t.port, t.tls), (6379, true));
    }

    #[test]
    fn parses_port_password_and_db() {
        assert_eq!(
            parse_url("redis://:sekret@10.0.0.5:6380/2").unwrap(),
            Target {
                host: "10.0.0.5".to_string(),
                port: 6380,
                user: None,
                password: Some("sekret".to_string()),
                db: Some(2),
                tls: false,
            }
        );
        // Userinfo without a colon is treated as the password.
        let t = parse_url("redis://sekret@host").unwrap();
        assert_eq!(t.user, None);
        assert_eq!(t.password, Some("sekret".to_string()));
        // A user:password pair keeps both (2-arg AUTH for ACL users).
        let t = parse_url("redis://app:sekret@host").unwrap();
        assert_eq!(t.user, Some("app".to_string()));
        assert_eq!(t.password, Some("sekret".to_string()));
        // Trailing slash without a db index is tolerated.
        assert_eq!(parse_url("redis://host/").unwrap().db, None);
    }

    #[test]
    fn rejects_bad_urls() {
        assert!(parse_url("http://host").is_err());
        assert!(parse_url("rediss://host:notaport").is_err());
        assert!(parse_url("redis://host:notaport").is_err());
        assert!(parse_url("redis://:pass@").is_err());
        assert!(parse_url("redis://host/notadb").is_err());
    }

    #[test]
    fn encodes_resp_commands() {
        assert_eq!(
            encode_command(&["SET", "k", "1", "EX", "60"]),
            b"*5\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\n1\r\n$2\r\nEX\r\n$2\r\n60\r\n"
        );
    }

    #[test]
    fn parses_replies() {
        let mut r = Cursor::new(b"+OK\r\n:1\r\n:0\r\n-ERR nope\r\n$3\r\nabc\r\n$-1\r\n".to_vec());
        assert_eq!(read_reply(&mut r).unwrap(), Reply::Simple("OK".to_string()));
        assert_eq!(read_reply(&mut r).unwrap(), Reply::Integer(1));
        assert_eq!(read_reply(&mut r).unwrap(), Reply::Integer(0));
        assert_eq!(
            read_reply(&mut r).unwrap(),
            Reply::Error("ERR nope".to_string())
        );
        assert_eq!(
            read_reply(&mut r).unwrap(),
            Reply::Bulk(Some(b"abc".to_vec()))
        );
        assert_eq!(read_reply(&mut r).unwrap(), Reply::Bulk(None));
    }

    #[test]
    fn truncated_reply_is_an_error() {
        let mut r = Cursor::new(b"+OK".to_vec());
        assert!(read_reply(&mut r).is_err());
        let mut r = Cursor::new(b"$5\r\nab\r\n".to_vec());
        assert!(read_reply(&mut r).is_err());
    }

    // ---- loopback fake-RESP server (same pattern as sns.rs's capture test) ----

    use std::sync::Arc;

    type Commands = Arc<Mutex<Vec<Vec<String>>>>;

    /// Serves one scripted reply list per accepted connection, recording every
    /// command received. Closes each connection after its script runs out.
    fn fake_redis(
        scripts: Vec<Vec<&'static [u8]>>,
    ) -> (Target, Commands, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let commands: Commands = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&commands);
        let handle = std::thread::spawn(move || {
            for script in scripts {
                let (sock, _) = listener.accept().expect("accept");
                let mut writer = sock.try_clone().expect("clone");
                let mut reader = BufReader::new(sock);
                for reply in script {
                    let argv = read_command(&mut reader);
                    recorded.lock().unwrap().push(argv);
                    writer.write_all(reply).expect("write reply");
                }
            }
        });
        let target = Target {
            host: "127.0.0.1".to_string(),
            port,
            user: None,
            password: None,
            db: None,
            tls: false,
        };
        (target, commands, handle)
    }

    /// Reads one RESP array-of-bulk-strings command.
    fn read_command(r: &mut impl BufRead) -> Vec<String> {
        let head = read_line(r).expect("command header");
        let n: usize = head
            .strip_prefix('*')
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("not an array header: {head:?}"));
        (0..n)
            .map(|_| {
                let len_line = read_line(r).expect("bulk header");
                let len: usize = len_line
                    .strip_prefix('$')
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| panic!("not a bulk header: {len_line:?}"));
                let mut buf = vec![0u8; len + 2];
                r.read_exact(&mut buf).expect("bulk payload");
                buf.truncate(len);
                String::from_utf8(buf).expect("utf8 arg")
            })
            .collect()
    }

    fn test_state(target: Target, ttl_seconds: u64) -> State {
        State {
            target,
            ttl_seconds,
            slot: Mutex::new(ConnSlot {
                conn: None,
                last_failure: None,
            }),
        }
    }

    #[test]
    fn auth_select_order_and_set_ex_over_loopback() {
        let (mut target, commands, server) = fake_redis(vec![vec![
            b"+OK\r\n", // AUTH
            b"+OK\r\n", // SELECT
            b":1\r\n",  // EXISTS
            b"+OK\r\n", // SET
        ]]);
        target.password = Some("sekret".to_string());
        target.db = Some(2);
        let st = test_state(target, 60);

        assert!(st.hit("abgen:hit:b:k"));
        st.mark("abgen:hit:b:k");
        server.join().expect("server");

        let got = commands.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                vec!["AUTH".to_string(), "sekret".to_string()],
                vec!["SELECT".to_string(), "2".to_string()],
                vec!["EXISTS".to_string(), "abgen:hit:b:k".to_string()],
                vec![
                    "SET".to_string(),
                    "abgen:hit:b:k".to_string(),
                    "1".to_string(),
                    "EX".to_string(),
                    "60".to_string(),
                ],
            ]
        );
    }

    #[test]
    fn acl_user_sends_two_arg_auth() {
        let (mut target, commands, server) = fake_redis(vec![vec![
            b"+OK\r\n", // AUTH user pass
            b":0\r\n",  // EXISTS
        ]]);
        target.user = Some("app".to_string());
        target.password = Some("sekret".to_string());
        let st = test_state(target, 60);

        assert!(!st.hit("k"));
        server.join().expect("server");

        assert_eq!(
            commands.lock().unwrap()[0],
            vec!["AUTH".to_string(), "app".to_string(), "sekret".to_string()]
        );
    }

    #[test]
    fn err_reply_fails_open_and_backs_off() {
        // Connection 1: EXISTS answered with -ERR (fail open, start backoff).
        // Connection 2: only reached after the backoff is rewound.
        let (target, commands, server) =
            fake_redis(vec![vec![b"-ERR nope\r\n"], vec![b":1\r\n"]]);
        let st = test_state(target, 60);

        assert!(!st.hit("k"), "-ERR must be a miss, never a hit");
        // Backoff active: no reconnect, still a miss, server sees nothing new.
        assert!(!st.hit("k"));
        assert_eq!(commands.lock().unwrap().len(), 1, "backoff must not reconnect");

        // Rewind the backoff window; the next probe reconnects and hits.
        st.slot.lock().unwrap().last_failure = Instant::now().checked_sub(FAILURE_BACKOFF);
        assert!(st.hit("k"));
        server.join().expect("server");
        assert_eq!(commands.lock().unwrap().len(), 2);
    }

    #[test]
    fn plain_connection_authenticates_selects_and_commands() {
        let listener = TcpListener::bind("localhost:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut writer = sock;
            let mut seen = Vec::new();
            while seen.len() < 3 {
                let args = read_command(&mut reader);
                let reply: &[u8] = if args[0] == "EXISTS" {
                    b":1\r\n"
                } else {
                    b"+OK\r\n"
                };
                writer.write_all(reply).unwrap();
                seen.push(args);
            }
            seen
        });

        let target = Target {
            host: "localhost".to_string(),
            port,
            user: None,
            password: Some("pw".to_string()),
            db: Some(3),
            tls: false,
        };
        let mut conn = connect(&target).unwrap();
        assert_eq!(conn.command(&["EXISTS", "k"]).unwrap(), Reply::Integer(1));

        let seen = server.join().unwrap();
        assert_eq!(seen[0], ["AUTH", "pw"]);
        assert_eq!(seen[1], ["SELECT", "3"]);
        assert_eq!(seen[2], ["EXISTS", "k"]);
    }

    /// No cert fixture here: the point is that `rediss://` reaches the wire as a
    /// TLS ClientHello (with SNI) instead of plaintext RESP, and that a peer
    /// which doesn't speak TLS is a clean error rather than a hang or a panic.
    #[test]
    fn tls_target_opens_with_a_client_hello() {
        let listener = TcpListener::bind("localhost:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).unwrap();
            buf.truncate(n);
            let _ = sock.write_all(b"-ERR this port is plaintext\r\n");
            buf
        });

        let target = Target {
            host: "localhost".to_string(),
            port,
            user: None,
            password: None,
            db: None,
            tls: true,
        };
        assert!(connect(&target).is_err());

        let hello = server.join().unwrap();
        assert_eq!(&hello[..3], &[0x16, 0x03, 0x01]);
        assert!(hello.windows(9).any(|w| w == b"localhost"));
    }
}
