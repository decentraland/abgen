use crate::space::Space;
use crate::Result;
use anyhow::{anyhow, Context};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const UPLOAD_RETRIES: u32 = 3;

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs_f64(0.5 * 2f64.powi(attempt as i32))
}

struct UploadJob {
    key: String,
    path: PathBuf,
}

#[derive(Default)]
pub struct UploadReport {
    pub ok: usize,
    pub failed: Vec<String>,
}

impl UploadReport {
    /// Hard gate before the corpus manifest: a manifest must never advertise
    /// a bundle that did not land, and a false success acks the SQS message.
    pub fn ensure_ok(&self) -> Result<()> {
        if self.failed.is_empty() {
            return Ok(());
        }
        Err(anyhow!(
            "{} bundle upload(s) failed after {UPLOAD_RETRIES} attempts each: {:?}",
            self.failed.len(),
            self.failed
        ))
    }
}

/// Bounded background PUT queue: `enqueue` hands off a finalized on-disk
/// bundle's key/path to a fixed worker pool, freeing the caller (a
/// conversion worker) immediately instead of blocking it on the network.
/// Workers re-read the file from disk rather than taking the bytes, so
/// queue depth never inflates process RSS beyond the file sizes already on
/// disk. `drain` closes the queue and blocks until every enqueued upload
/// has been attempted (with retries) — callers use it as the barrier that
/// must complete before anything that depends on "every bundle is durably
/// stored" (the corpus manifest) is written or published.
pub struct UploadPool {
    tx: SyncSender<UploadJob>,
    workers: Vec<JoinHandle<()>>,
    ok: Arc<AtomicUsize>,
    failed: Arc<Mutex<Vec<String>>>,
}

impl UploadPool {
    pub fn new(space: Arc<Space>, workers: usize) -> Self {
        let workers = workers.max(1);
        let (tx, rx) = sync_channel::<UploadJob>(workers * 4);
        let rx = Arc::new(Mutex::new(rx));
        let ok = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(Mutex::new(Vec::new()));
        let handles = (0..workers)
            .map(|_| spawn_worker(rx.clone(), space.clone(), ok.clone(), failed.clone()))
            .collect();
        Self {
            tx,
            workers: handles,
            ok,
            failed,
        }
    }

    pub fn enqueue(&self, key: String, path: PathBuf) {
        if self.tx.send(UploadJob { key, path }).is_err() {
            tracing::warn!("upload pool queue closed; dropping enqueue");
        }
    }

    pub fn drain(self) -> UploadReport {
        let UploadPool {
            tx,
            workers,
            ok,
            failed,
        } = self;
        drop(tx);
        for w in workers {
            let _ = w.join();
        }
        UploadReport {
            ok: ok.load(Ordering::Relaxed),
            failed: Arc::try_unwrap(failed)
                .map(|m| m.into_inner().unwrap())
                .unwrap_or_default(),
        }
    }
}

fn spawn_worker(
    rx: Arc<Mutex<Receiver<UploadJob>>>,
    space: Arc<Space>,
    ok: Arc<AtomicUsize>,
    failed: Arc<Mutex<Vec<String>>>,
) -> JoinHandle<()> {
    std::thread::spawn(move || loop {
        let job = {
            let guard = rx.lock().unwrap();
            guard.recv()
        };
        let Ok(job) = job else { break };
        match upload_with_retries(&space, &job) {
            Ok(()) => {
                ok.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                tracing::warn!(
                    key = %job.key,
                    error = %format!("{e:#}"),
                    "upload pool put failed after retries"
                );
                metrics::counter!("abgen_upload_failed_total").increment(1);
                failed.lock().unwrap().push(job.key.clone());
            }
        }
    })
}

fn upload_with_retries(space: &Space, job: &UploadJob) -> Result<()> {
    let bytes = std::fs::read(&job.path)
        .with_context(|| format!("upload pool read {}", job.path.display()))?;
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..UPLOAD_RETRIES {
        match space.put_timed(&job.key, &bytes) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        if attempt + 1 < UPLOAD_RETRIES {
            std::thread::sleep(backoff(attempt));
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("upload {} failed", job.key)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("abgen-upload-test-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn stub_space(host: &str) -> Arc<Space> {
        Arc::new(Space::with_static_creds(
            "http",
            host,
            "us-east-1",
            None,
            false,
            false,
            "AKIATEST",
            "secret",
        ))
    }

    #[test]
    fn drain_waits_for_all_enqueued_uploads_and_reports_ok() {
        let (host, seen) = crate::live::stub::serve(vec![
            ("/v1/a.bin".to_string(), 200, Vec::new()),
            ("/v1/b.bin".to_string(), 200, Vec::new()),
        ]);
        let dir = temp_dir("drain-ok");
        let pa = dir.join("a.bin");
        let pb = dir.join("b.bin");
        std::fs::write(&pa, b"AAAA").unwrap();
        std::fs::write(&pb, b"BBBB").unwrap();

        let pool = UploadPool::new(stub_space(&host), 2);
        pool.enqueue("v1/a.bin".to_string(), pa);
        pool.enqueue("v1/b.bin".to_string(), pb);
        let report = pool.drain();

        assert_eq!(report.ok, 2);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(report.ensure_ok().is_ok());
        let mut log = seen.lock().unwrap().clone();
        log.sort();
        assert_eq!(
            log,
            vec!["PUT /v1/a.bin".to_string(), "PUT /v1/b.bin".to_string()]
        );
    }

    #[test]
    fn failed_put_retries_bounded_times_and_surfaces_in_report() {
        let (host, seen) =
            crate::live::stub::serve(vec![("/v1/ok.bin".to_string(), 200, Vec::new())]);
        let dir = temp_dir("drain-fail");
        let pok = dir.join("ok.bin");
        let pbad = dir.join("bad.bin");
        std::fs::write(&pok, b"OK").unwrap();
        std::fs::write(&pbad, b"BAD").unwrap();

        let pool = UploadPool::new(stub_space(&host), 2);
        pool.enqueue("v1/ok.bin".to_string(), pok);
        pool.enqueue("v1/missing.bin".to_string(), pbad);
        let report = pool.drain();

        assert_eq!(report.ok, 1);
        assert_eq!(report.failed, vec!["v1/missing.bin".to_string()]);
        let log = seen.lock().unwrap().clone();
        let bad_attempts = log.iter().filter(|l| *l == "PUT /v1/missing.bin").count();
        assert_eq!(bad_attempts, UPLOAD_RETRIES as usize);

        let err = report.ensure_ok().unwrap_err().to_string();
        assert!(
            err.contains("v1/missing.bin") && err.contains("1 bundle upload(s) failed"),
            "gate must name the failed key: {err}"
        );
    }

    #[test]
    fn read_only_space_put_fails_without_retrying_forever() {
        let (host, seen) = crate::live::stub::serve(vec![]);
        let space = Arc::new(Space::with_static_creds(
            "http",
            &host,
            "us-east-1",
            None,
            false,
            true,
            "AKIATEST",
            "secret",
        ));
        let dir = temp_dir("drain-ro");
        let p = dir.join("x.bin");
        std::fs::write(&p, b"X").unwrap();

        let pool = UploadPool::new(space, 1);
        pool.enqueue("v1/x.bin".to_string(), p);
        let report = pool.drain();

        assert_eq!(report.ok, 0);
        assert_eq!(report.failed, vec!["v1/x.bin".to_string()]);
        assert!(seen.lock().unwrap().is_empty());
    }
}
