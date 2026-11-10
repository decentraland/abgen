//! The publisher/worker split: decoding and publishing in separate functions, each with
//! only the access its half needs.
//!
//! One function converts in `ABGEN_LAMBDA_MODE=convert`: it fetches the entity, decodes
//! its glTF, Draco, JPEG and font bytes in-process, writes bundles and manifests through
//! to the CDN bucket as it builds, and publishes the finished event. Split, the two halves
//! run in different functions built from the same image:
//!
//! - **publisher** (`ABGEN_LAMBDA_MODE=publisher`, the SQS consumer) decides what to
//!   convert (the already-converted check), asks the worker with a synchronous `Invoke`,
//!   and publishes what comes back: it copies the staged objects into the CDN bucket
//!   server-side ([`abgen::space::Space::copy_from`]), then the scene sources, markers,
//!   satellite record and finished event exactly as before. It never decodes a creator
//!   file (the scene sources it uploads are copied as they are).
//! - **worker** (`ABGEN_LAMBDA_MODE=worker`) converts one entity per request into a
//!   **scratch** bucket, under a prefix the publisher chose for this job, and answers
//!   with the objects it wrote. Its view of the CDN is read-only (the per-file reuse
//!   probes); it needs no write access to the CDN, no Redis and no SNS. The publisher
//!   invokes it with the entity id as the Lambda tenant, and the function is created with
//!   tenant isolation, so an execution environment that converted one entity never
//!   converts another: nothing carries over between entities, disk caches included.
//!
//! The publisher checks the answer before it copies anything ([`KeyPolicy`]): a key is
//! copied only when it is one this entity's conversion may produce — a bundle named after
//! one of the entity's own content hashes, under the lane its type selects and in the
//! placement its type selects, or this entity's manifest for a platform it asked for —
//! and the whole answer is refused otherwise. A digest-named shared bundle that already
//! exists on the CDN is never replaced (a conditional copy), as in `convert` mode, where
//! the build reuses those and never rewrites them. The job prefix carries a nonce that is
//! fresh per attempt, invoke retries included, so two attempts never share staged objects.
//!
//! Failure semantics are the single function's: a worker that errors, times out or
//! answers badly fails the job, which SQS redelivers and, on the final receive, the
//! publisher tombstones. The LOD lane does not run in publisher mode yet (it decodes
//! creator assets too); publisher mode refuses to start with `ENABLE_LODS` on.

use crate::config::Config;
use crate::convert::{EntityOutcome, PlatformOutcome};
use abgen::lambda_invoke::{invocation_remaining, LambdaInvoker};
use abgen::live::Proxy;
use abgen::space::{Space, WrittenObject};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Name of the worker function the publisher converts through.
pub const WORKER_FUNCTION_ENV: &str = "ABGEN_WORKER_FUNCTION";
/// Overrides the Lambda endpoint (tests, local stacks; plain http only for loopback).
pub const WORKER_ENDPOINT_ENV: &str = "ABGEN_WORKER_ENDPOINT";
/// Seconds the publisher waits for one conversion, cold start and retries included
/// (default 810). The worker function's own timeout must not be longer.
pub const WORKER_TIMEOUT_ENV: &str = "ABGEN_WORKER_TIMEOUT_S";
/// The scratch bucket: the worker stages into it, the publisher copies out of it. Same
/// endpoint, region and credentials as the CDN space (`ABGEN_S3_*`), path-style addressed.
pub const SCRATCH_BUCKET_ENV: &str = "ABGEN_SCRATCH_S3_BUCKET";
/// Every job's staging prefix starts here; the worker role may put under it and nowhere
/// else.
pub const SCRATCH_ROOT: &str = "jobs";

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(810);
/// Marks a convert request or response and versions its shape.
const PROTOCOL_KEY: &str = "abgenConvert";
const PROTOCOL_VERSION: u64 = 1;
/// Time the publisher keeps after the worker answers: the copies into the CDN, the scene
/// sources, markers and the finished event.
const PUBLISHER_RESERVE: Duration = Duration::from_secs(90);
/// Below this, the publisher does not start a call at all.
const MIN_CALL_BUDGET: Duration = Duration::from_secs(30);
/// Copies stop, and the job fails whole, when the invocation has less than this left: a
/// job that times out mid-copy on its final receive would reach the DLQ without a
/// tombstone.
const COPY_TIME_FLOOR: Duration = Duration::from_secs(20);
/// What the worker may answer with: Lambda's 6 MB less some room.
const MAX_RESPONSE_BYTES: usize = 6_000_000;
/// S3 copies one object of at most 5 GB in a single request.
const MAX_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// More than a worker's disk can hold: a job over this is not a conversion.
const MAX_TOTAL_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// The largest scenes stage a few thousand objects.
const MAX_OBJECTS: usize = 10_000;
const COPY_WORKERS: usize = 8;
const COPY_RETRIES: u32 = 3;

/// Where one job is staged: a bucket and a `jobs/<entity id>/<nonce>` prefix of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staging {
    pub bucket: String,
    pub prefix: String,
}

impl Staging {
    fn for_job(bucket: &str, entity_id: &str) -> Staging {
        Staging {
            bucket: bucket.to_string(),
            prefix: format!("{SCRATCH_ROOT}/{entity_id}/{}", nonce()),
        }
    }

    /// The wire key of `key` staged under this prefix.
    pub fn staged_key(&self, key: &str) -> String {
        format!("{}/{key}", self.prefix)
    }
}

/// 32 hex characters, fresh per job. From the kernel's random source; if that cannot be
/// read, from two freshly keyed hashes.
fn nonce() -> String {
    let mut bytes = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes));
    if read.is_err() {
        use std::hash::BuildHasher;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let a = std::collections::hash_map::RandomState::new().hash_one(std::process::id());
        let b = std::collections::hash_map::RandomState::new().hash_one(nanos);
        bytes[..8].copy_from_slice(&a.to_le_bytes());
        bytes[8..].copy_from_slice(&b.to_le_bytes());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `prefix` is `jobs/<entity_id>/<32 hex>`, the only shape a worker stages under.
fn valid_prefix(prefix: &str, entity_id: &str) -> bool {
    let mut segments = prefix.split('/');
    segments.next() == Some(SCRATCH_ROOT)
        && segments.next() == Some(entity_id)
        && segments
            .next()
            .is_some_and(|n| n.len() == 32 && n.bytes().all(|b| b.is_ascii_hexdigit()))
        && segments.next().is_none()
}

/// The invoke payload: what to convert, from where, into which staging prefix.
pub fn request(
    entity_id: &str,
    content_server: &str,
    platforms: &[String],
    staging: &Staging,
) -> Value {
    json!({
        PROTOCOL_KEY: PROTOCOL_VERSION,
        "entityId": entity_id,
        "contentServer": content_server,
        "platforms": platforms,
        "scratch": { "bucket": staging.bucket, "prefix": staging.prefix },
    })
}

/// Whether an invocation payload is a convert request.
pub fn is_request(event: &Value) -> bool {
    event.get(PROTOCOL_KEY).is_some()
}

fn manifest_key(entity_id: &str, platform: &str) -> String {
    format!("manifest/{entity_id}_{platform}.json")
}

fn scratch_bucket_from_env() -> Result<String> {
    std::env::var(SCRATCH_BUCKET_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("{SCRATCH_BUCKET_ENV} is not set"))
}

/// The CDN space from the environment, which both halves need: the worker to probe,
/// the publisher to write and to reach the scratch bucket beside it.
fn cdn_space_from_env(mode: &str) -> Result<Space> {
    let space = Space::from_env().ok_or_else(|| {
        anyhow!(
            "{mode} mode needs the CDN space (ABGEN_S3_ENDPOINT, ABGEN_S3_BUCKET and \
             credentials)"
        )
    })?;
    if space.bucket.is_none() {
        bail!("{mode} mode needs ABGEN_S3_BUCKET");
    }
    if !space.path_style {
        bail!(
            "{mode} mode needs ABGEN_S3_PATH_STYLE=1: the scratch bucket is addressed by \
             path on the same endpoint"
        );
    }
    Ok(space)
}

// ---------------------------------------------------------------------------------------
// Worker side
// ---------------------------------------------------------------------------------------

/// What a worker needs from its environment, built once at start-up.
pub struct WorkerEnv {
    /// The CDN, read-only: reads and the per-file reuse probes.
    cdn: Arc<Space>,
    scratch_bucket: String,
}

static WORKER: OnceLock<WorkerEnv> = OnceLock::new();

impl WorkerEnv {
    pub(crate) fn new(cdn: Space, scratch_bucket: &str) -> Result<WorkerEnv> {
        if cdn.bucket.is_none() {
            bail!("worker mode needs ABGEN_S3_BUCKET");
        }
        if !cdn.read_only {
            bail!(
                "worker mode needs ABGEN_S3_READ_ONLY=1: it reads the CDN and writes only \
                 to scratch"
            );
        }
        if !cdn.path_style {
            bail!("worker mode needs ABGEN_S3_PATH_STYLE=1: scratch is addressed by path");
        }
        Ok(WorkerEnv {
            cdn: Arc::new(cdn),
            scratch_bucket: scratch_bucket.to_string(),
        })
    }

    fn from_env() -> Result<WorkerEnv> {
        let cdn = cdn_space_from_env("worker")?;
        WorkerEnv::new(cdn, &scratch_bucket_from_env().context("worker mode")?)
    }
}

/// Worker start-up: refuses to serve without a content server allowlist, a read-only,
/// path-style view of the CDN and a scratch bucket, and installs them for [`serve_worker`].
pub fn worker_preflight(cfg: &Config) -> Result<()> {
    match cfg.allowed_content_server_hosts.as_deref() {
        Some(hosts) if !hosts.is_empty() => {}
        _ => bail!(
            "worker mode needs ALLOWED_CONTENT_SERVER_HOSTS: it fetches whatever content \
             server a request names"
        ),
    }
    let env = WorkerEnv::from_env()?;
    if std::env::var("ABGEN_SNS_TOPIC_ARN").is_ok_and(|v| !v.trim().is_empty()) {
        eprintln!("worker: ABGEN_SNS_TOPIC_ARN is set, but a worker publishes no event; ignored");
    }
    let _ = WORKER.set(env);
    Ok(())
}

/// A convert request, checked.
pub struct ConvertRequest {
    pub entity_id: String,
    pub content_server: String,
    pub platforms: Vec<String>,
    pub staging: Staging,
}

/// Every field of the request, checked before anything is fetched: the entity id, the
/// content server (https, a public name, the allowlist), platforms this worker builds,
/// this worker's own scratch bucket and a prefix of the right shape for the entity.
fn check_request(cfg: &Config, env: &WorkerEnv, event: &Value) -> Result<ConvertRequest> {
    let version = event.get(PROTOCOL_KEY).and_then(Value::as_u64);
    if version != Some(PROTOCOL_VERSION) {
        bail!("unsupported {PROTOCOL_KEY} version {version:?}");
    }
    let entity_id = crate::event::validated_entity_id(
        event
            .get("entityId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("convert request has no entityId"))?,
    )?;
    let content_server = event
        .get("contentServer")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("convert request has no contentServer"))?;
    crate::event::validate_content_server(
        content_server,
        cfg.allowed_content_server_hosts.as_deref(),
    )?;
    let platforms = requested_platforms(cfg, event.get("platforms"))?;
    let bucket = event
        .pointer("/scratch/bucket")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("convert request has no scratch.bucket"))?;
    if bucket != env.scratch_bucket {
        bail!(
            "convert request names scratch bucket {bucket:?}; this worker stages into {:?}",
            env.scratch_bucket
        );
    }
    let prefix = event
        .pointer("/scratch/prefix")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("convert request has no scratch.prefix"))?;
    if !valid_prefix(prefix, &entity_id) {
        bail!("convert request prefix {prefix:?} is not {SCRATCH_ROOT}/{entity_id}/<nonce>");
    }
    Ok(ConvertRequest {
        entity_id,
        content_server: content_server.to_string(),
        platforms,
        staging: Staging {
            bucket: bucket.to_string(),
            prefix: prefix.to_string(),
        },
    })
}

/// Worker side: checks the request, converts the entity into the staging prefix and
/// answers with what it wrote.
pub fn serve_worker(cfg: &Config, event: &Value) -> Result<Value> {
    let env = WORKER
        .get()
        .ok_or_else(|| anyhow!("worker not initialised (worker_preflight was not run)"))?;
    let request = check_request(cfg, env, event)?;
    stage(cfg, env, &request)
}

/// The conversion's output directory, removed before the build and again when the
/// request ends, however it ends. The build uploads a bundle only when its file is not
/// already in the directory, which is right when one process converts and publishes
/// but not when a failed attempt's files are still there for the next one: that attempt
/// would stage a manifest naming bundles it never uploaded.
struct FreshOutputDir {
    dir: PathBuf,
    keep: bool,
}

impl FreshOutputDir {
    fn new(cfg: &Config, entity_id: &str) -> Result<FreshOutputDir> {
        let dir = cfg
            .out_root
            .join(&*abgen::naming::fs_safe_component(entity_id));
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("clear stale output {}", dir.display()))?;
        }
        Ok(FreshOutputDir {
            dir,
            keep: cfg.keep_output,
        })
    }
}

impl Drop for FreshOutputDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// Converts `request` with two spaces — the CDN for reads and probes, the staging prefix
/// for every put — and answers with the per-platform outcome and the objects written.
pub(crate) fn stage(cfg: &Config, env: &WorkerEnv, request: &ConvertRequest) -> Result<Value> {
    let ConvertRequest {
        entity_id,
        content_server,
        platforms,
        staging,
    } = request;
    // The scratch view is derived from the CDN space built at start-up, never from the
    // environment again: same endpoint and credentials, this job's bucket and prefix.
    let scratch = Arc::new(
        env.cdn
            .same_endpoint()
            .writable_bucket(&staging.bucket)?
            .with_key_prefix(&staging.prefix)
            .logging_writes(),
    );
    let proxy = crate::convert::make_proxy_with_spaces(
        cfg,
        content_server,
        env.cdn.clone(),
        scratch.clone(),
    );
    let _output = FreshOutputDir::new(cfg, entity_id)?;
    let outcome =
        crate::convert::convert_entity(cfg, &proxy, entity_id, content_server, platforms)?;
    let objects = scratch.written_objects();
    // The build logs a manifest put that fails and carries on; here that is a failed job,
    // because the publisher would publish bundles no manifest names.
    for platform in platforms {
        let manifest = manifest_key(entity_id, platform);
        if !objects.iter().any(|o| o.key == manifest) {
            bail!("{entity_id} {platform}: the manifest was not staged ({manifest})");
        }
    }
    let value = response(&outcome, &objects);
    let size = serde_json::to_vec(&value)?.len();
    if size > MAX_RESPONSE_BYTES {
        bail!(
            "{entity_id}: {} staged object(s) make a {size}-byte response, over the \
             {MAX_RESPONSE_BYTES} bytes a Lambda response may carry",
            objects.len()
        );
    }
    eprintln!(
        "worker: {entity_id} staged {} object(s) under {}/{}",
        objects.len(),
        staging.bucket,
        staging.prefix
    );
    Ok(value)
}

fn requested_platforms(cfg: &Config, value: Option<&Value>) -> Result<Vec<String>> {
    let list = value
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("convert request has no platforms list"))?;
    let mut out: Vec<String> = Vec::new();
    for platform in list {
        let platform = platform
            .as_str()
            .ok_or_else(|| anyhow!("convert request platform is not a string"))?;
        if !cfg.platforms.iter().any(|known| known == platform) {
            bail!(
                "convert request asks for platform {platform:?}, which this worker does not \
                 build (PLATFORMS={})",
                cfg.platforms.join(",")
            );
        }
        if !out.iter().any(|o| o == platform) {
            out.push(platform.to_string());
        }
    }
    if out.is_empty() {
        bail!("convert request asks for no platform");
    }
    Ok(out)
}

fn response(outcome: &EntityOutcome, objects: &[WrittenObject]) -> Value {
    json!({
        PROTOCOL_KEY: PROTOCOL_VERSION,
        "entityId": outcome.entity_id,
        "contentServer": outcome.content_server,
        "platforms": outcome.platforms.iter().map(|p| json!({
            "platform": p.platform,
            "bundles": p.bundles,
            "exitCode": p.exit_code,
        })).collect::<Vec<_>>(),
        "texEncodeCache": { "hits": outcome.cache_hits, "misses": outcome.cache_misses },
        "objects": objects.iter().map(|o| json!({ "key": o.key, "size": o.bytes })).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------------------
// Publisher side
// ---------------------------------------------------------------------------------------

/// The publisher's client for the worker function, built once per process.
pub struct Publisher {
    invoker: LambdaInvoker,
    timeout: Duration,
    scratch_bucket: String,
    /// The scratch bucket, for deleting what a job staged once it is on the CDN.
    scratch: Space,
}

static PUBLISHER: OnceLock<Publisher> = OnceLock::new();

/// Builds the publisher side from the environment and installs it for [`publisher`].
/// Publisher mode calls it once at start-up; nothing is installed in any other mode, so
/// `ABGEN_WORKER_FUNCTION` alone changes nothing.
pub fn init_publisher() -> Result<()> {
    let function = std::env::var(WORKER_FUNCTION_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("publisher mode needs {WORKER_FUNCTION_ENV}"))?;
    let region = std::env::var("AWS_REGION")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "us-east-1".to_string());
    let endpoint = std::env::var(WORKER_ENDPOINT_ENV).ok();
    let timeout = abgen::lambda_invoke::parse_timeout_setting(
        WORKER_TIMEOUT_ENV,
        std::env::var(WORKER_TIMEOUT_ENV).ok().as_deref(),
        DEFAULT_TIMEOUT,
    )?;
    let scratch_bucket = scratch_bucket_from_env().context("publisher mode")?;
    let publisher = Publisher::new(
        &function,
        &region,
        endpoint.as_deref(),
        timeout,
        &scratch_bucket,
    )?;
    eprintln!(
        "init: conversions run in {function} (wait {}s), staged in {scratch_bucket}",
        timeout.as_secs()
    );
    let _ = PUBLISHER.set(publisher);
    Ok(())
}

/// The installed publisher side; `None` outside publisher mode.
pub fn publisher() -> Option<&'static Publisher> {
    PUBLISHER.get()
}

/// Which keys one entity's conversion may publish. Built by the publisher from the entity
/// document it fetched itself, never from anything the worker says.
pub struct KeyPolicy {
    entity_id: String,
    /// The version lane the entity's type selects (`Proxy::version_for`).
    lane: String,
    /// Scenes place bundles under the shared `{lane}/assets/`, everything else under
    /// `{lane}/{entityId}/`.
    scene: bool,
    platforms: Vec<String>,
    /// The entity's content hashes, as listed and lowercased (the mac lane lowercases
    /// them): a bundle is named after one of these and nothing else.
    hashes: HashSet<String>,
}

enum KeyKind {
    /// `{lane}/assets/{name}`; `digest_named` when the name carries a recipe digest, which
    /// is what `convert` mode reuses off the CDN and never rewrites.
    SharedBundle {
        digest_named: bool,
    },
    EntityBundle,
    Manifest,
}

impl KeyPolicy {
    pub fn new(entity_id: &str, lane: &str, entity_doc: &Value, platforms: &[String]) -> KeyPolicy {
        let mut hashes = HashSet::new();
        for hash in entity_doc
            .get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|c| c.get("hash").and_then(Value::as_str))
        {
            hashes.insert(hash.to_string());
            hashes.insert(hash.to_ascii_lowercase());
        }
        KeyPolicy {
            entity_id: entity_id.to_string(),
            lane: lane.to_string(),
            scene: crate::output::entity_type(entity_doc) == "scene",
            platforms: platforms.to_vec(),
            hashes,
        }
    }

    pub fn entity_id(&self) -> &str {
        &self.entity_id
    }

    pub fn platforms(&self) -> &[String] {
        &self.platforms
    }

    /// What `key` is, if this entity's conversion may publish it: a bundle under the
    /// entity's lane, in the placement its type selects, named `{hash}_{platform}` or
    /// `{hash}_{digest}_{platform}` after one of its content hashes for a requested
    /// platform; or its manifest for a requested platform. Anything else is `None`.
    fn classify(&self, key: &str) -> Option<KeyKind> {
        if let Some(rest) = key.strip_prefix("manifest/") {
            let stem = rest.strip_suffix(".json")?;
            // Entity ids are alphanumeric, so the first `_` ends the id.
            let (id, platform) = stem.split_once('_')?;
            return (id == self.entity_id && self.platforms.iter().any(|p| p == platform))
                .then_some(KeyKind::Manifest);
        }
        let mut segments = key.split('/');
        let lane = segments.next()?;
        let scope = segments.next()?;
        let name = segments.next()?;
        if segments.next().is_some() || lane != self.lane {
            return None;
        }
        let (hash, digest_named) = self.parse_bundle_name(name)?;
        if !self.hashes.contains(hash) {
            return None;
        }
        if scope == "assets" && self.scene {
            Some(KeyKind::SharedBundle { digest_named })
        } else if scope == self.entity_id && !self.scene {
            Some(KeyKind::EntityBundle)
        } else {
            None
        }
    }

    /// `{hash}_{platform}` or `{hash}_{digest}_{platform}` for a requested platform: the
    /// hash and whether a 32-hex digest is there. Anything else is `None`.
    fn parse_bundle_name<'a>(&self, name: &'a str) -> Option<(&'a str, bool)> {
        let stem = self
            .platforms
            .iter()
            .find_map(|p| name.strip_suffix(p.as_str())?.strip_suffix('_'))?;
        let (hash, digest) = match stem.split_once('_') {
            Some((hash, digest)) => (hash, Some(digest)),
            None => (stem, None),
        };
        // Content hashes are CIDs, alphanumeric. The `b64-` content-versioned ids of a local
        // preview build (`naming::is_content_versioned_id`) are not: they never appear in
        // an entity a content server deploys, so they never reach the publisher.
        if hash.is_empty() || !hash.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        match digest {
            None => Some((hash, false)),
            Some(d) if d.len() == 32 && d.bytes().all(|b| b.is_ascii_hexdigit()) => {
                Some((hash, true))
            }
            Some(_) => None,
        }
    }
}

/// One object a worker staged, by its CDN key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedObject {
    pub key: String,
    /// Copied only if nothing is at the key yet: a digest-named shared bundle, which the
    /// build reuses off the CDN and never rewrites.
    pub only_if_absent: bool,
}

/// A conversion the worker finished and staged, checked and ready to publish.
#[derive(Debug)]
pub struct Staged {
    pub staging: Staging,
    /// What the rest of the job reads, as if the conversion had run here.
    pub outcome: EntityOutcome,
    pub bundles: Vec<StagedObject>,
    pub manifests: Vec<StagedObject>,
}

/// What [`Publisher::publish`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Published {
    pub copied: usize,
    /// Digest-named shared bundles already on the CDN, left as they were.
    pub already_present: usize,
    pub deleted: usize,
}

fn shown(key: &str) -> String {
    key.chars().take(120).collect()
}

impl Publisher {
    pub(crate) fn new(
        function: &str,
        region: &str,
        endpoint: Option<&str>,
        timeout: Duration,
        scratch_bucket: &str,
    ) -> Result<Publisher> {
        let invoker = LambdaInvoker::new(function, region, endpoint)?;
        let scratch = cdn_space_from_env("publisher")?.writable_bucket(scratch_bucket)?;
        Ok(Publisher {
            invoker,
            timeout,
            scratch_bucket: scratch_bucket.to_string(),
            scratch,
        })
    }

    /// Converts the policy's platforms of its entity in the worker and returns what it
    /// staged, checked ([`Staged`]). Every attempt, invoke retries included, names a
    /// fresh staging prefix, so a retry never shares objects with a run still writing.
    pub fn convert(
        &self,
        cfg: &Config,
        policy: &KeyPolicy,
        content_server: &str,
    ) -> Result<Staged> {
        let entity_id = policy.entity_id();
        let budget = abgen::lambda_invoke::call_budget(
            self.timeout,
            invocation_remaining(),
            PUBLISHER_RESERVE,
            MIN_CALL_BUDGET,
        )
        .with_context(|| format!("convert {entity_id}"))?;
        let started = Instant::now();
        let mut staging: Option<Staging> = None;
        let response = self
            .invoker
            .invoke_with(
                &mut |_| {
                    let attempt = Staging::for_job(&self.scratch_bucket, entity_id);
                    let payload = request(entity_id, content_server, policy.platforms(), &attempt);
                    staging = Some(attempt);
                    payload
                },
                budget,
                // One tenant per entity: the worker function is created with tenant
                // isolation, so an execution environment that converted this entity never
                // converts another one, disk caches included.
                Some(entity_id),
            )
            .with_context(|| format!("convert {entity_id} in {}", self.invoker.function()))?;
        let staging = staging.ok_or_else(|| anyhow!("no attempt was made"))?;
        eprintln!(
            "publisher: {entity_id} converted by {} in {:.1}s",
            self.invoker.function(),
            started.elapsed().as_secs_f64()
        );
        parse_response(cfg, policy, content_server, staging, &response)
    }

    /// Moves a staged conversion into the CDN: bundles first, manifests last, so no
    /// manifest ever names a bundle that is not there yet; digest-named shared bundles
    /// with a conditional copy that leaves an existing object alone. Then the staged
    /// objects are deleted, best effort: the scratch bucket's lifecycle rule sweeps
    /// whatever this misses. Fails whole, and copies nothing more, when the invocation
    /// is about to run out.
    pub fn publish(&self, proxy: &Proxy, staged: &Staged) -> Result<Published> {
        let staging = &staged.staging;
        let (copied_bundles, already_present) = copy_all(proxy, staging, &staged.bundles)?;
        let (copied_manifests, _) = copy_all(proxy, staging, &staged.manifests)?;
        let deleted = AtomicUsize::new(0);
        let all: Vec<&StagedObject> = staged.bundles.iter().chain(&staged.manifests).collect();
        for_each_parallel(&all, |object| {
            match self.scratch.delete(&staging.staged_key(&object.key)) {
                Ok(()) => {
                    deleted.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => eprintln!(
                    "publisher: staged {} not deleted ({e:#}); the lifecycle rule will",
                    object.key
                ),
            }
            Ok(())
        })?;
        let published = Published {
            copied: copied_bundles + copied_manifests,
            already_present,
            deleted: deleted.load(Ordering::Relaxed),
        };
        eprintln!(
            "publisher: {} copied {} object(s) from {}/{}, {} shared bundle(s) already on the \
             CDN, {} staged object(s) deleted",
            staged.outcome.entity_id,
            published.copied,
            staging.bucket,
            staging.prefix,
            published.already_present,
            published.deleted
        );
        Ok(published)
    }
}

/// Checks the worker's answer: the entity and platforms asked for, standing exit codes,
/// and only keys the policy allows, every requested platform's manifest among them.
fn parse_response(
    cfg: &Config,
    policy: &KeyPolicy,
    content_server: &str,
    staging: Staging,
    value: &Value,
) -> Result<Staged> {
    let entity_id = policy.entity_id();
    let platforms = policy.platforms();
    let version = value.get(PROTOCOL_KEY).and_then(Value::as_u64);
    if version != Some(PROTOCOL_VERSION) {
        bail!("worker answered with {PROTOCOL_KEY} version {version:?}, not a convert response");
    }
    let answered = value.get("entityId").and_then(Value::as_str);
    if answered != Some(entity_id) {
        bail!("worker answered for entity {answered:?}, asked for {entity_id}");
    }

    let list = value
        .get("platforms")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("worker response has no platforms"))?;
    let mut outcomes: Vec<PlatformOutcome> = Vec::with_capacity(list.len());
    for entry in list {
        let platform = entry
            .get("platform")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("worker response platform has no name"))?;
        if !platforms.iter().any(|p| p == platform)
            || outcomes.iter().any(|o| o.platform == platform)
        {
            bail!("worker answered for platform {platform:?}, which was not asked for");
        }
        let exit_code = entry
            .get("exitCode")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("worker response for {platform} has no exitCode"))?;
        // A conversion that fails for real errors out of the worker; an outcome carries a
        // standing code or it is not one (`convert::exit_code_tests`).
        if exit_code != 0
            && exit_code != i64::from(abgen::manifest::EXIT_CONVERSION_ERRORS_TOLERATED)
        {
            bail!("worker reported exit code {exit_code} for {platform}, not a standing code");
        }
        let bundles = entry
            .get("bundles")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("worker response for {platform} has no bundle count"))?;
        if bundles > MAX_OBJECTS as u64 {
            bail!("worker reports {bundles} bundles for {platform}, more than a job may publish");
        }
        outcomes.push(PlatformOutcome {
            platform: platform.to_string(),
            bundles: bundles as usize,
            exit_code: exit_code as i32,
        });
    }
    for platform in platforms {
        if !outcomes.iter().any(|o| &o.platform == platform) {
            bail!("worker answered nothing for platform {platform}");
        }
    }

    let objects = value
        .get("objects")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("worker response has no objects"))?;
    if objects.len() > MAX_OBJECTS {
        bail!(
            "worker staged {} objects, over the {MAX_OBJECTS} a job may publish",
            objects.len()
        );
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut bundles = Vec::new();
    let mut manifests = Vec::new();
    let mut total_bytes: u64 = 0;
    for object in objects {
        let key = object
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("worker response object has no key"))?;
        let size = object
            .get("size")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("worker response object {:?} has no size", shown(key)))?;
        if size > MAX_OBJECT_BYTES {
            bail!(
                "worker staged {} at {size} bytes, over the {MAX_OBJECT_BYTES} S3 copies in \
                 one request",
                shown(key)
            );
        }
        total_bytes = total_bytes.saturating_add(size);
        if total_bytes > MAX_TOTAL_BYTES {
            bail!("worker staged over {MAX_TOTAL_BYTES} bytes in all, more than a job may publish");
        }
        let kind = policy.classify(key).ok_or_else(|| {
            anyhow!(
                "worker staged {:?}, which is not a key {entity_id}'s conversion may publish",
                shown(key)
            )
        })?;
        if !seen.insert(key.to_string()) {
            continue;
        }
        let object = StagedObject {
            key: key.to_string(),
            only_if_absent: matches!(kind, KeyKind::SharedBundle { digest_named: true }),
        };
        match kind {
            KeyKind::Manifest => manifests.push(object),
            KeyKind::SharedBundle { .. } | KeyKind::EntityBundle => bundles.push(object),
        }
    }
    for platform in platforms {
        let manifest = manifest_key(entity_id, platform);
        if !manifests.iter().any(|o| o.key == manifest) {
            bail!("worker staged no manifest for {entity_id} {platform}");
        }
    }

    let cache = |name: &str| {
        value
            .pointer(&format!("/texEncodeCache/{name}"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Ok(Staged {
        staging,
        outcome: EntityOutcome {
            entity_id: entity_id.to_string(),
            content_server: content_server.to_string(),
            cid_dir: cfg
                .out_root
                .join(&*abgen::naming::fs_safe_component(entity_id)),
            platforms: outcomes,
            cache_hits: cache("hits"),
            cache_misses: cache("misses"),
        },
        bundles,
        manifests,
    })
}

/// Copies `objects` into the CDN; `(copied, left alone as already present)`.
fn copy_all(proxy: &Proxy, staging: &Staging, objects: &[StagedObject]) -> Result<(usize, usize)> {
    let copied = AtomicUsize::new(0);
    let present = AtomicUsize::new(0);
    for_each_parallel(objects, |object| {
        if let Some(left) = invocation_remaining() {
            if left < COPY_TIME_FLOOR {
                bail!(
                    "{}s left in this invocation with objects still to copy; failing the job \
                     so it is retried whole",
                    left.as_secs()
                );
            }
        }
        if copy_with_retries(proxy, staging, object)? {
            copied.fetch_add(1, Ordering::Relaxed);
        } else {
            present.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    })?;
    Ok((
        copied.load(Ordering::Relaxed),
        present.load(Ordering::Relaxed),
    ))
}

fn copy_with_retries(proxy: &Proxy, staging: &Staging, object: &StagedObject) -> Result<bool> {
    let source = staging.staged_key(&object.key);
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..COPY_RETRIES {
        match proxy.space_copy_from(&staging.bucket, &source, &object.key, object.only_if_absent) {
            Ok(copied) => return Ok(copied),
            Err(e) => last = Some(e),
        }
        if attempt + 1 < COPY_RETRIES {
            std::thread::sleep(Duration::from_millis(500 * (1u64 << attempt)));
        }
    }
    Err(last
        .unwrap_or_else(|| anyhow!("copy {} failed", object.key))
        .context(format!("copy {} into the CDN", object.key)))
}

/// Runs `f` over `items` on up to [`COPY_WORKERS`] threads, stopping at the first error.
fn for_each_parallel<T: Sync>(items: &[T], f: impl Fn(&T) -> Result<()> + Sync) -> Result<()> {
    let next = AtomicUsize::new(0);
    let failed: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let workers = COPY_WORKERS.min(items.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                if failed.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                    break;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(i) else { break };
                if let Err(e) = f(item) {
                    let mut slot = failed.lock().unwrap_or_else(|e| e.into_inner());
                    if slot.is_none() {
                        *slot = Some(e);
                    }
                    break;
                }
            });
        }
    });
    match failed.into_inner().unwrap_or_else(|e| e.into_inner()) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::output::tests::{point_space_at, serve};

    pub(crate) fn cfg(tag: &str) -> Config {
        let root =
            std::env::temp_dir().join(format!("abgen-split-test-{tag}-{}", std::process::id()));
        Config {
            platforms: vec!["windows".to_string(), "mac".to_string()],
            version: "v49".to_string(),
            wearable_version: "v49w".to_string(),
            cache_dir: root.join("cache").to_string_lossy().into_owned(),
            default_content_server: "https://peer.decentraland.org/content".to_string(),
            out_root: root.join("out"),
            keep_output: false,
            allowed_content_server_hosts: Some(vec!["peer.decentraland.org".to_string()]),
            http_secret: None,
            lods_enabled: false,
            max_receive_count: 3,
            lod_levels: vec![1],
        }
    }

    pub(crate) const ENTITY: &str = "bafkreientity";
    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    const DIGEST: &str = "0123456789abcdef0123456789abcdef";

    fn staging() -> Staging {
        Staging {
            bucket: "scratch".to_string(),
            prefix: format!("jobs/{ENTITY}/{NONCE}"),
        }
    }

    fn platforms() -> Vec<String> {
        vec!["windows".to_string(), "mac".to_string()]
    }

    /// A scene whose content is one hash, `Qmhash`.
    fn scene_doc() -> Value {
        json!({ "type": "scene", "pointers": ["0,0"], "content": [{"file": "a.glb", "hash": "Qmhash"}] })
    }

    fn scene_policy() -> KeyPolicy {
        KeyPolicy::new(ENTITY, "v49", &scene_doc(), &platforms())
    }

    /// A wearable whose content is `Qmhash`, under the wearable lane.
    pub(crate) fn wearable_doc() -> Value {
        json!({ "type": "wearable", "pointers": ["urn:x"], "content": [{"file": "w.glb", "hash": "Qmhash"}] })
    }

    fn wearable_policy() -> KeyPolicy {
        KeyPolicy::new(ENTITY, "v49w", &wearable_doc(), &platforms())
    }

    /// A good answer for the scene: a digest-named and a bare shared bundle, two manifests,
    /// one of them twice.
    fn good_response() -> Value {
        json!({
            "abgenConvert": 1,
            "entityId": ENTITY,
            "contentServer": "https://peer.decentraland.org/content",
            "platforms": [
                {"platform": "windows", "bundles": 3, "exitCode": 0},
                {"platform": "mac", "bundles": 3, "exitCode": 12},
            ],
            "texEncodeCache": {"hits": 4, "misses": 2},
            "objects": [
                {"key": format!("v49/assets/Qmhash_{DIGEST}_windows"), "size": 10},
                {"key": "v49/assets/qmhash_mac", "size": 10},
                {"key": format!("manifest/{ENTITY}_windows.json"), "size": 100},
                {"key": format!("manifest/{ENTITY}_mac.json"), "size": 100},
                {"key": format!("manifest/{ENTITY}_mac.json"), "size": 100},
            ],
        })
    }

    #[test]
    fn request_json_is_pinned() {
        let request = request(ENTITY, "https://c/content", &platforms(), &staging());
        assert_eq!(
            request.to_string(),
            format!(
                "{{\"abgenConvert\":1,\"entityId\":\"{ENTITY}\",\"contentServer\":\"https://c/content\",\
                 \"platforms\":[\"windows\",\"mac\"],\"scratch\":{{\"bucket\":\"scratch\",\
                 \"prefix\":\"jobs/{ENTITY}/{NONCE}\"}}}}"
            )
        );
        assert!(is_request(&request));
        assert!(!is_request(&json!({"entityId": ENTITY})));
        assert!(!is_request(&json!({"abgenScenerun": 2})));
    }

    #[test]
    fn a_job_is_staged_under_its_own_nonce() {
        let a = Staging::for_job("scratch", ENTITY);
        let b = Staging::for_job("scratch", ENTITY);
        assert!(valid_prefix(&a.prefix, ENTITY), "{}", a.prefix);
        assert_ne!(a.prefix, b.prefix, "two jobs never share a prefix");
        assert_eq!(
            a.staged_key("manifest/x.json"),
            format!("{}/manifest/x.json", a.prefix)
        );

        assert!(valid_prefix(&format!("jobs/{ENTITY}/{NONCE}"), ENTITY));
        for bad in [
            format!("jobs/other/{NONCE}"),
            format!("jobs/{ENTITY}/{NONCE}/more"),
            format!("jobs/{ENTITY}/short"),
            format!("jobs/{ENTITY}/{}", "g".repeat(32)),
            format!("other/{ENTITY}/{NONCE}"),
            format!("../{ENTITY}/{NONCE}"),
            format!("jobs/{ENTITY}"),
        ] {
            assert!(!valid_prefix(&bad, ENTITY), "{bad}");
        }
    }

    #[test]
    fn a_scene_may_publish_shared_bundles_named_after_its_own_hashes() {
        let policy = scene_policy();
        let kind = |key: &str| policy.classify(key);
        assert!(matches!(
            kind(&format!("v49/assets/Qmhash_{DIGEST}_windows")),
            Some(KeyKind::SharedBundle { digest_named: true })
        ));
        assert!(matches!(
            kind("v49/assets/qmhash_mac"),
            Some(KeyKind::SharedBundle {
                digest_named: false
            })
        ));
        assert!(matches!(
            kind(&format!("manifest/{ENTITY}_mac.json")),
            Some(KeyKind::Manifest)
        ));
        for bad in [
            // Another entity's hash, however well formed: not this conversion's to publish.
            format!("v49/assets/Qmother_{DIGEST}_windows"),
            "v49/assets/Qmother_windows".to_string(),
            // A scene's bundles are shared, never entity-scoped.
            format!("v49/{ENTITY}/Qmhash_windows"),
            // The wearable lane is not a scene's.
            format!("v49w/assets/Qmhash_{DIGEST}_windows"),
            "v48/assets/Qmhash_windows".to_string(),
            // Not bundle names: no platform, an unrequested platform, a short digest, dots.
            "v49/assets/Qmhash".to_string(),
            "v49/assets/Qmhash_webgl".to_string(),
            "v49/assets/Qmhash_abc_windows".to_string(),
            "v49/assets/x.js".to_string(),
            "v49/assets/Qmhash_windows.js".to_string(),
            "v49/assets/dir/Qmhash_windows".to_string(),
            "v49/assets/".to_string(),
            "v49/assets/..".to_string(),
            "v49/assets/Qm/../x".to_string(),
            "manifest/other_mac.json".to_string(),
            format!("manifest/{ENTITY}_webgl.json"),
            format!("manifest/{ENTITY}_mac.txt"),
            format!("LOD/1/{ENTITY}_1_windows"),
            format!("SatelliteMaps/rebuild/1-{ENTITY}.json"),
            "lod-reuse/by-inputs/abc.json".to_string(),
            format!("{ENTITY}/main.crdt"),
            String::new(),
        ] {
            assert!(kind(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_wearable_may_publish_only_its_own_entity_scoped_bundles() {
        let policy = wearable_policy();
        assert!(matches!(
            policy.classify(&format!("v49w/{ENTITY}/Qmhash_windows")),
            Some(KeyKind::EntityBundle)
        ));
        assert!(matches!(
            policy.classify(&format!("v49w/{ENTITY}/qmhash_mac")),
            Some(KeyKind::EntityBundle)
        ));
        for bad in [
            // Not shared, not the scene lane, not another entity's folder.
            "v49w/assets/Qmhash_windows".to_string(),
            format!("v49/{ENTITY}/Qmhash_windows"),
            "v49w/bafkother/Qmhash_windows".to_string(),
            format!("v49w/{ENTITY}/Qmother_windows"),
        ] {
            assert!(policy.classify(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_good_answer_sorts_bundles_from_manifests_and_dedupes() {
        let staged = parse_response(
            &cfg("good"),
            &scene_policy(),
            "https://peer.decentraland.org/content",
            staging(),
            &good_response(),
        )
        .unwrap();
        assert_eq!(staged.staging, staging());
        assert_eq!(staged.outcome.entity_id, ENTITY);
        assert_eq!(staged.outcome.exit_code(), 12);
        assert_eq!(
            (staged.outcome.cache_hits, staged.outcome.cache_misses),
            (4, 2)
        );
        assert_eq!(staged.outcome.platforms.len(), 2);
        assert_eq!(
            staged.bundles,
            vec![
                StagedObject {
                    key: format!("v49/assets/Qmhash_{DIGEST}_windows"),
                    only_if_absent: true
                },
                StagedObject {
                    key: "v49/assets/qmhash_mac".into(),
                    only_if_absent: false
                },
            ]
        );
        assert_eq!(
            staged
                .manifests
                .iter()
                .map(|o| o.key.as_str())
                .collect::<Vec<_>>(),
            vec![
                format!("manifest/{ENTITY}_windows.json"),
                format!("manifest/{ENTITY}_mac.json"),
            ]
        );
    }

    #[test]
    fn the_workers_answer_is_checked_before_anything_is_copied() {
        let check = |mutate: &dyn Fn(&mut Value)| {
            let mut value = good_response();
            mutate(&mut value);
            parse_response(
                &cfg("checked"),
                &scene_policy(),
                "https://peer.decentraland.org/content",
                staging(),
                &value,
            )
            .map(|_| ())
            .unwrap_err()
            .to_string()
        };
        assert!(check(&|v| v["abgenConvert"] = json!(2)).contains("version"));
        assert!(check(&|v| v["entityId"] = json!("bafkother")).contains("asked for"));
        assert!(check(&|v| v["platforms"][1]["exitCode"] = json!(5)).contains("standing code"));
        assert!(
            check(&|v| v["platforms"][1]["platform"] = json!("webgl")).contains("not asked for")
        );
        assert!(
            check(&|v| v["platforms"][1]["platform"] = json!("windows")).contains("not asked for")
        );
        assert!(
            check(&|v| v["platforms"][1]["bundles"] = json!(u64::MAX)).contains("more than a job")
        );
        assert!(check(&|v| {
            v["platforms"].as_array_mut().unwrap().pop();
        })
        .contains("nothing for platform mac"));
        assert!(
            check(&|v| v["objects"][0]["key"] = json!("manifest/bafkother_windows.json"))
                .contains("not a key")
        );
        assert!(check(
            &|v| v["objects"][0]["key"] = json!(format!("v49/assets/Qmother_{DIGEST}_windows"))
        )
        .contains("not a key"));
        assert!(
            check(&|v| v["objects"][0]["size"] = json!(MAX_OBJECT_BYTES + 1)).contains("over the")
        );
        assert!(check(&|v| {
            for object in v["objects"].as_array_mut().unwrap() {
                object["size"] = json!(MAX_OBJECT_BYTES);
            }
        })
        .contains("in all"));
        assert!(check(&|v| {
            v["objects"].as_array_mut().unwrap().truncate(3);
        })
        .contains("no manifest for"));
        assert!(check(&|v| v["objects"] = json!(null)).contains("no objects"));
    }

    fn worker_env(host: &str) -> WorkerEnv {
        let cdn = Space::with_static_creds(
            "http",
            host,
            "us-east-1",
            Some("cdn"),
            true,
            true,
            "AKIATEST",
            "secret",
        );
        WorkerEnv::new(cdn, "scratch").unwrap()
    }

    #[test]
    fn worker_requests_are_checked_before_anything_is_fetched() {
        let cfg = cfg("requests");
        let env = worker_env("127.0.0.1:9");
        let err = |event: Value| {
            check_request(&cfg, &env, &event)
                .map(|_| ())
                .unwrap_err()
                .to_string()
        };
        let good = request(
            ENTITY,
            "https://peer.decentraland.org/content",
            &platforms(),
            &staging(),
        );

        assert!(check_request(&cfg, &env, &good).is_ok());
        assert!(err(json!({"abgenConvert": 0})).contains("version"));
        let mut bad_host = good.clone();
        bad_host["contentServer"] = json!("https://evil.example.org/content");
        assert!(err(bad_host).contains("ALLOWED_CONTENT_SERVER_HOSTS"));
        let mut bad_id = good.clone();
        bad_id["entityId"] = json!("../x");
        assert!(err(bad_id).contains("invalid entityId"));
        let mut bad_platform = good.clone();
        bad_platform["platforms"] = json!(["windows", "webgl"]);
        assert!(err(bad_platform).contains("does not build"));
        let mut no_platform = good.clone();
        no_platform["platforms"] = json!([]);
        assert!(err(no_platform).contains("no platform"));
        let mut other_bucket = good.clone();
        other_bucket["scratch"]["bucket"] = json!("elsewhere");
        assert!(err(other_bucket).contains("this worker stages into"));
        let mut other_prefix = good.clone();
        other_prefix["scratch"]["prefix"] = json!(format!("jobs/bafkother/{NONCE}"));
        assert!(err(other_prefix).contains("is not jobs/"));
    }

    #[test]
    fn a_worker_env_needs_a_read_only_path_style_cdn() {
        let space = |path_style: bool, read_only: bool| {
            Space::with_static_creds(
                "http",
                "127.0.0.1:9",
                "us-east-1",
                Some("cdn"),
                path_style,
                read_only,
                "AKIATEST",
                "secret",
            )
        };
        // `Space` has no Debug, so the errors are read out by hand.
        let refused = |path_style: bool, read_only: bool| match WorkerEnv::new(
            space(path_style, read_only),
            "scratch",
        ) {
            Ok(_) => panic!("accepted path_style={path_style} read_only={read_only}"),
            Err(e) => e.to_string(),
        };
        assert!(WorkerEnv::new(space(true, true), "scratch").is_ok());
        let err = refused(true, false);
        assert!(err.contains("ABGEN_S3_READ_ONLY"), "{err}");
        let err = refused(false, true);
        assert!(err.contains("ABGEN_S3_PATH_STYLE"), "{err}");

        let mut cfg = cfg("preflight");
        cfg.allowed_content_server_hosts = None;
        let err = worker_preflight(&cfg).unwrap_err().to_string();
        assert!(err.contains("ALLOWED_CONTENT_SERVER_HOSTS"), "{err}");
    }

    #[test]
    fn the_output_dir_is_fresh_for_every_request_and_gone_after() {
        let cfg = cfg("fresh-dir");
        let dir = cfg.out_root.join(ENTITY);
        std::fs::create_dir_all(dir.join("windows")).unwrap();
        std::fs::write(dir.join("windows").join("stale_windows"), b"old").unwrap();
        {
            let _fresh = FreshOutputDir::new(&cfg, ENTITY).unwrap();
            assert!(
                !dir.exists(),
                "a previous attempt's files are gone before the build"
            );
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("new"), b"x").unwrap();
        }
        assert!(!dir.exists(), "and the build's own files are gone after");

        let mut keep = cfg;
        keep.keep_output = true;
        {
            let _fresh = FreshOutputDir::new(&keep, ENTITY).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
        }
        assert!(dir.exists(), "KEEP_OUTPUT keeps the build's files");
        let _ = std::fs::remove_dir_all(&keep.out_root);
    }

    /// A 1×1 RGBA PNG, built by hand: one stored deflate block, so no encoder is needed.
    pub(crate) fn tiny_png() -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut crc = 0xFFFF_FFFFu32;
            for &b in data {
                crc ^= u32::from(b);
                for _ in 0..8 {
                    crc = if crc & 1 == 1 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend((data.len() as u32).to_be_bytes());
            out.extend(kind);
            out.extend(data);
            let mut body = kind.to_vec();
            body.extend(data);
            out.extend(crc32(&body).to_be_bytes());
        }
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend(1u32.to_be_bytes());
        ihdr.extend(1u32.to_be_bytes());
        ihdr.extend([8, 6, 0, 0, 0]);
        chunk(&mut png, b"IHDR", &ihdr);
        // zlib: header, one final stored block (filter byte + one opaque red pixel), adler32.
        let raw = [0u8, 255, 0, 0, 255];
        let (mut a, mut b) = (1u32, 0u32);
        for &x in &raw {
            a = (a + u32::from(x)) % 65521;
            b = (b + a) % 65521;
        }
        let mut idat = vec![0x78, 0x01, 0x01];
        idat.extend((raw.len() as u16).to_le_bytes());
        idat.extend((!(raw.len() as u16)).to_le_bytes());
        idat.extend(raw);
        idat.extend(((b << 16) | a).to_be_bytes());
        chunk(&mut png, b"IDAT", &idat);
        chunk(&mut png, b"IEND", &[]);
        png
    }

    /// A full worker run against a fake content server, CDN and scratch bucket: a scene
    /// with one image. The first request fails on the mac platform after staging windows;
    /// the second, for windows only, must stage the windows bundle again rather than find
    /// the previous attempt's file on disk and skip the upload.
    #[test]
    fn a_failed_attempt_leaves_nothing_the_next_one_trusts() {
        let hash = "qmimg";
        let digest = abgen::naming::image_class_digest(false, false, false, ".png");
        let name = |platform: &str| format!("{hash}_{digest}_{platform}");
        let staged = |key: &str| format!("/scratch/jobs/{ENTITY}/{NONCE}/{key}");
        let doc = json!({
            "type": "scene", "pointers": ["0,0"],
            "content": [{"file": "tex/img.png", "hash": hash}],
        });
        let (host, seen) = serve(vec![
            (
                format!("/contents/{ENTITY}"),
                200,
                serde_json::to_vec(&doc).unwrap(),
            ),
            (format!("/contents/{hash}"), 200, tiny_png()),
            (
                staged(&format!("v49/assets/{}", name("windows"))),
                200,
                Vec::new(),
            ),
            (
                staged(&format!("manifest/{ENTITY}_windows.json")),
                200,
                Vec::new(),
            ),
            // The mac bundle upload fails every time: attempt one dies here.
            (
                staged(&format!("v49/assets/{}", name("mac"))),
                500,
                Vec::new(),
            ),
            (
                staged(&format!("manifest/{ENTITY}_mac.json")),
                200,
                Vec::new(),
            ),
        ]);
        let _space = point_space_at(&host);
        let cfg = cfg("retry");
        let env = worker_env(&host);
        let content_server = format!("http://{host}");

        let first = ConvertRequest {
            entity_id: ENTITY.to_string(),
            content_server: content_server.clone(),
            platforms: platforms(),
            staging: staging(),
        };
        let err = stage(&cfg, &env, &first).unwrap_err();
        assert!(format!("{err:#}").contains("upload"), "{err:#}");
        let after_first = seen.lock().unwrap().clone();
        assert!(
            after_first.contains(&format!(
                "PUT {}",
                staged(&format!("v49/assets/{}", name("windows")))
            )),
            "{after_first:?}"
        );
        assert!(
            !cfg.out_root.join(ENTITY).exists(),
            "the failed attempt's output is gone"
        );

        let second = ConvertRequest {
            platforms: vec!["windows".to_string()],
            ..first
        };
        let value = stage(&cfg, &env, &second).unwrap();
        let keys: Vec<&str> = value["objects"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|o| o["key"].as_str())
            .collect();
        assert!(
            keys.contains(&format!("v49/assets/{}", name("windows")).as_str()),
            "the windows bundle is staged again: {keys:?}"
        );
        assert!(
            keys.contains(&format!("manifest/{ENTITY}_windows.json").as_str()),
            "{keys:?}"
        );
        assert_eq!(value["platforms"][0]["exitCode"], 0);
        assert_eq!(value["platforms"][0]["bundles"], 1);
        let puts = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|s| {
                *s == &format!("PUT {}", staged(&format!("v49/assets/{}", name("windows"))))
            })
            .count();
        assert_eq!(puts, 2, "one upload per attempt");
        // The policy the publisher would apply accepts exactly what was staged.
        let policy = KeyPolicy::new(ENTITY, "v49", &doc, &["windows".to_string()]);
        for key in &keys {
            assert!(policy.classify(key).is_some(), "{key}");
        }
        let _ = std::fs::remove_dir_all(cfg.out_root.parent().unwrap());
    }

    #[test]
    fn timeouts_and_budgets() {
        let s = Duration::from_secs;
        assert_eq!(
            abgen::lambda_invoke::call_budget(
                s(810),
                Some(s(900)),
                PUBLISHER_RESERVE,
                MIN_CALL_BUDGET
            )
            .unwrap(),
            s(810)
        );
        assert_eq!(
            abgen::lambda_invoke::call_budget(
                s(810),
                Some(s(300)),
                PUBLISHER_RESERVE,
                MIN_CALL_BUDGET
            )
            .unwrap(),
            s(210)
        );
        assert!(abgen::lambda_invoke::call_budget(
            s(810),
            Some(s(100)),
            PUBLISHER_RESERVE,
            MIN_CALL_BUDGET
        )
        .is_err());
    }

    pub(crate) fn publisher_at(host: &str) -> Publisher {
        Publisher::new(
            "abgen-worker",
            "us-east-1",
            Some(&format!("http://{host}")),
            Duration::from_secs(60),
            "scratch",
        )
        .unwrap()
    }

    fn wearable_staged(bundles: Vec<StagedObject>, manifests: Vec<StagedObject>) -> Staged {
        let mut response = good_response();
        response["objects"] = json!([
            {"key": format!("v49w/{ENTITY}/Qmhash_windows"), "size": 10},
            {"key": format!("manifest/{ENTITY}_windows.json"), "size": 100},
            {"key": format!("manifest/{ENTITY}_mac.json"), "size": 100},
        ]);
        let outcome = parse_response(
            &cfg("staged"),
            &wearable_policy(),
            "https://c/content",
            staging(),
            &response,
        )
        .unwrap()
        .outcome;
        Staged {
            staging: staging(),
            outcome,
            bundles,
            manifests,
        }
    }

    #[test]
    fn publish_copies_bundles_before_manifests_then_deletes_the_staging() {
        // The fake answers every path 200: copies and deletes alike.
        let (host, seen) = serve(vec![("/".to_string(), 200, Vec::new())]);
        let _space = point_space_at(&host);
        std::env::set_var("ABGEN_S3_BUCKET", "cdn");
        let publisher = publisher_at(&host);
        let proxy = crate::convert::make_proxy(&cfg("publish"), "https://c/content");
        std::env::remove_var("ABGEN_S3_BUCKET");
        let staged = wearable_staged(
            vec![
                StagedObject {
                    key: format!("v49w/{ENTITY}/Qma_windows"),
                    only_if_absent: false,
                },
                StagedObject {
                    key: format!("v49w/{ENTITY}/Qmb_windows"),
                    only_if_absent: false,
                },
            ],
            vec![StagedObject {
                key: format!("manifest/{ENTITY}_windows.json"),
                only_if_absent: false,
            }],
        );

        let published = publisher.publish(&proxy, &staged).unwrap();
        assert_eq!(
            published,
            Published {
                copied: 3,
                already_present: 0,
                deleted: 3
            }
        );

        let seen = seen.lock().unwrap().clone();
        let puts: Vec<&String> = seen.iter().filter(|s| s.starts_with("PUT ")).collect();
        assert_eq!(puts.len(), 3, "{seen:?}");
        assert_eq!(
            puts[2],
            &format!("PUT /cdn/manifest/{ENTITY}_windows.json"),
            "the manifest is copied after every bundle: {seen:?}"
        );
        for key in ["Qma_windows", "Qmb_windows"] {
            assert!(
                seen.contains(&format!(
                    "DELETE /scratch/jobs/{ENTITY}/{NONCE}/v49w/{ENTITY}/{key}"
                )),
                "{seen:?}"
            );
        }
        assert!(
            seen.contains(&format!(
                "DELETE /scratch/jobs/{ENTITY}/{NONCE}/manifest/{ENTITY}_windows.json"
            )),
            "{seen:?}"
        );
        assert!(
            !seen.iter().any(|s| s.starts_with("HEAD ")),
            "nothing is probed: {seen:?}"
        );
    }

    #[test]
    fn a_digest_named_bundle_already_on_the_cdn_is_left_alone() {
        let key = format!("v49/assets/Qmhash_{DIGEST}_windows");
        // The conditional copy of that key is refused (412); everything else is accepted.
        let (host, seen) = serve(vec![
            (format!("/cdn/{key}"), 412, Vec::new()),
            ("/".to_string(), 200, Vec::new()),
        ]);
        let _space = point_space_at(&host);
        std::env::set_var("ABGEN_S3_BUCKET", "cdn");
        let publisher = publisher_at(&host);
        let proxy = crate::convert::make_proxy(&cfg("present"), "https://c/content");
        std::env::remove_var("ABGEN_S3_BUCKET");
        let staged = wearable_staged(
            vec![
                StagedObject {
                    key: key.clone(),
                    only_if_absent: true,
                },
                StagedObject {
                    key: "v49/assets/qmhash_mac".to_string(),
                    only_if_absent: false,
                },
            ],
            vec![],
        );

        let published = publisher.publish(&proxy, &staged).unwrap();
        assert_eq!(
            published,
            Published {
                copied: 1,
                already_present: 1,
                deleted: 2
            }
        );
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.contains(&format!("PUT /cdn/{key}")),
            "the copy was attempted, conditionally: {seen:?}"
        );
        assert!(
            seen.contains(&"PUT /cdn/v49/assets/qmhash_mac".to_string()),
            "{seen:?}"
        );
        assert!(
            seen.contains(&format!("DELETE /scratch/jobs/{ENTITY}/{NONCE}/{key}")),
            "{seen:?}"
        );
    }

    #[test]
    fn a_failed_copy_fails_the_job_before_any_manifest_is_copied() {
        // Nothing accepts a PUT: the first bundle copy fails after its retries.
        let (host, seen) = serve(vec![]);
        let _space = point_space_at(&host);
        std::env::set_var("ABGEN_S3_BUCKET", "cdn");
        let publisher = publisher_at(&host);
        let proxy = crate::convert::make_proxy(&cfg("failed-copy"), "https://c/content");
        std::env::remove_var("ABGEN_S3_BUCKET");
        let staged = wearable_staged(
            vec![StagedObject {
                key: format!("v49w/{ENTITY}/Qma_windows"),
                only_if_absent: false,
            }],
            vec![StagedObject {
                key: format!("manifest/{ENTITY}_windows.json"),
                only_if_absent: false,
            }],
        );
        let err = publisher.publish(&proxy, &staged).unwrap_err();
        assert!(format!("{err:#}").contains("into the CDN"), "{err:#}");
        let seen = seen.lock().unwrap().clone();
        assert!(
            !seen.iter().any(|s| s.contains("manifest/")),
            "no manifest copy, no delete: {seen:?}"
        );
    }

    #[test]
    fn convert_sends_the_request_and_checks_the_answer() {
        let invoke_path = "/2015-03-31/functions/abgen-worker/invocations".to_string();
        let mut bad = good_response();
        bad["objects"][0]["key"] = json!("manifest/bafkother_windows.json");
        let (host, seen) = serve(vec![(
            invoke_path.clone(),
            200,
            bad.to_string().into_bytes(),
        )]);
        {
            let _space = point_space_at(&host);
            std::env::set_var("ABGEN_S3_BUCKET", "cdn");
            let publisher = publisher_at(&host);
            std::env::remove_var("ABGEN_S3_BUCKET");
            let err = publisher
                .convert(
                    &cfg("convert-bad"),
                    &scene_policy(),
                    "https://peer.decentraland.org/content",
                )
                .unwrap_err();
            assert!(format!("{err:#}").contains("not a key"), "{err:#}");
        }
        // The entity is the invoke's tenant: on a worker created with tenant isolation, the
        // execution environment that converts it never converts another entity.
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![format!("POST {invoke_path}"), format!("TENANT {ENTITY}")]
        );

        let (host, _) = serve(vec![(
            invoke_path,
            200,
            good_response().to_string().into_bytes(),
        )]);
        let _space = point_space_at(&host);
        std::env::set_var("ABGEN_S3_BUCKET", "cdn");
        let publisher = publisher_at(&host);
        std::env::remove_var("ABGEN_S3_BUCKET");
        let staged = publisher
            .convert(
                &cfg("convert-good"),
                &scene_policy(),
                "https://peer.decentraland.org/content",
            )
            .unwrap();
        assert_eq!(staged.staging.bucket, "scratch");
        assert!(
            valid_prefix(&staged.staging.prefix, ENTITY),
            "{}",
            staged.staging.prefix
        );
        assert_eq!(staged.bundles.len(), 2);
        assert_eq!(staged.manifests.len(), 2);
    }
}
