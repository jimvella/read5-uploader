//! The upload-and-delete half of the pipeline. It walks `out/`, PUTs pending
//! archives oldest first with bounded concurrency, marks them, keeps the
//! receiver document in sync, and removes minutes once every archive is
//! confirmed. Nothing here knows what is inside an archive; the caller
//! supplies the object key scheme and the "is this minute fully sealed"
//! rule.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use super::time::now_ms;
use super::{Backoff, Spool};
use crate::ingest::{Ingest, Refused};

/// `(channel, minute, receiver) -> object key`.
pub type KeyFn = Box<dyn Fn(&str, &str, &str) -> String + Send + Sync>;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct DriverStats {
    pub uploaded: u64,
    pub upload_failures: u64,
    pub deleted_minutes: u64,
    pub last_upload_ok_ms: Option<i64>,
    pub last_error: Option<String>,
}

struct Doc {
    path: PathBuf,
    content_type: String,
    dirty: bool,
}

pub struct Driver {
    spool: Spool,
    store: Option<Ingest>,
    receiver: String,
    key: KeyFn,
    concurrency: usize,
    delete: bool,
    /// An archive held back until the rest of its minute is uploaded.
    last: Option<String>,
    backoff: Backoff,
    docs: BTreeMap<String, Doc>,
    pub stats: DriverStats,
}

impl Driver {
    /// `store = None` is a dry run: nothing is uploaded or deleted.
    pub fn new(
        spool: Spool,
        store: Option<Ingest>,
        receiver: impl Into<String>,
        key: KeyFn,
        concurrency: usize,
        delete: bool,
    ) -> Self {
        Self {
            spool,
            store,
            receiver: receiver.into(),
            key,
            concurrency: concurrency.max(1),
            delete,
            last: None,
            backoff: Backoff::new(Duration::from_secs(2), Duration::from_secs(300)),
            docs: BTreeMap::new(),
            stats: DriverStats::default(),
        }
    }

    /// Upload `channel`'s archive only once every other archive of its minute
    /// is confirmed. For a marker whose presence vouches for the others.
    pub fn upload_last(&mut self, channel: impl Into<String>) {
        self.last = Some(channel.into());
    }

    pub fn key_for(&self, channel: &str, minute: &str) -> String {
        (self.key)(channel, minute, &self.receiver)
    }

    /// Register (or re-register) a small document to keep in sync with the server.
    /// It is uploaded whole whenever it is marked dirty.
    pub fn set_doc(&mut self, key: impl Into<String>, path: PathBuf, content_type: &str) {
        let key = key.into();
        self.backoff.clear(&key);
        self.docs.insert(
            key,
            Doc {
                path,
                content_type: content_type.to_string(),
                dirty: true,
            },
        );
    }

    pub fn mark_doc_dirty(&mut self, key: &str) {
        if let Some(d) = self.docs.get_mut(key) {
            d.dirty = true;
            self.backoff.clear(key);
        }
    }

    /// True when no registered document is waiting to be uploaded.
    pub fn docs_uploaded(&self) -> bool {
        self.docs.values().all(|d| !d.dirty)
    }

    /// Upload dirty documents, then pending archives oldest first, bounded
    /// by `concurrency`. Failures back off per key.
    pub async fn upload_due(&mut self) -> Result<()> {
        let Some(store) = self.store.clone() else {
            return Ok(());
        };
        let now = Instant::now();

        let dirty: Vec<String> = self
            .docs
            .iter()
            .filter(|(k, d)| d.dirty && self.backoff.ready(k, now))
            .map(|(k, _)| k.clone())
            .collect();
        for key in dirty {
            let (path, ct) = {
                let d = &self.docs[&key];
                (d.path.clone(), d.content_type.clone())
            };
            let bytes = match fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    self.note_failure(&key, &anyhow::Error::from(e).context("read document"));
                    continue;
                }
            };
            match store.put_verified(&key, bytes, &ct).await {
                Ok(()) => {
                    self.docs.get_mut(&key).expect("registered").dirty = false;
                    self.backoff.clear(&key);
                    info!(key, "uploaded document");
                }
                Err(e) => self.note_failure(&key, &e),
            }
        }

        let mut pending: Vec<(String, String)> = Vec::new();
        for mk in self.spool.list_out_minutes()? {
            let archives = self.spool.list_archives(&mk)?;
            for ch in due(&archives, self.last.as_deref()) {
                pending.push((mk.clone(), ch));
            }
        }
        let ready: Vec<(String, String, String)> = pending
            .into_iter()
            .map(|(mk, ch)| {
                let key = self.key_for(&ch, &mk);
                (mk, ch, key)
            })
            .filter(|(_, _, key)| self.backoff.ready(key, now))
            .collect();

        for chunk in ready.chunks(self.concurrency) {
            let mut set = tokio::task::JoinSet::new();
            for (i, (mk, ch, key)) in chunk.iter().enumerate() {
                let key = key.clone();
                let path = self.spool.zip_path(mk, ch);
                let store = store.clone();
                set.spawn(async move {
                    let bytes = match tokio::fs::read(&path).await {
                        Ok(b) => b,
                        Err(e) => return (i, key, Err(anyhow::Error::from(e).context("read zip"))),
                    };
                    let res = store.put_verified(&key, bytes, "application/zip").await;
                    (i, key, res)
                });
            }
            let mut results: Vec<Option<(String, Result<()>)>> =
                chunk.iter().map(|_| None).collect();
            while let Some(joined) = set.join_next().await {
                let (i, key, res) = joined.context("upload task panicked")?;
                results[i] = Some((key, res));
            }
            for ((mk, ch, _), slot) in chunk.iter().zip(results) {
                let (key, res) = slot.expect("every task reports");
                match res {
                    Ok(()) => {
                        let from = self.spool.zip_path(mk, ch);
                        let to = self.spool.uploaded_path(mk, ch);
                        fs::rename(&from, &to)
                            .with_context(|| format!("mark uploaded {}", to.display()))?;
                        self.backoff.clear(&key);
                        self.stats.uploaded += 1;
                        self.stats.last_upload_ok_ms = Some(now_ms());
                        info!(key, "uploaded");
                    }
                    Err(e) => self.note_failure(&key, &e),
                }
            }
        }
        Ok(())
    }

    fn note_failure(&mut self, key: &str, e: &anyhow::Error) {
        // A refusal will not change in seconds: wait five minutes.
        let refused = e.downcast_ref::<Refused>().is_some();
        let floor = if refused { Duration::from_secs(300) } else { Duration::ZERO };
        let n = self.backoff.failed(key, Instant::now(), floor);
        self.stats.upload_failures += 1;
        self.stats.last_error = Some(format!("{key}: {e:#}"));
        if refused {
            warn!(key, attempts = n, error = %format!("{e:#}"), "upload refused; retrying in 5 minutes");
        } else if n >= 3 {
            warn!(key, attempts = n, error = %format!("{e:#}"), "upload failing");
        } else {
            debug!(key, attempts = n, error = %format!("{e:#}"), "upload failed; will retry");
        }
    }

    /// Remove every minute whose archives are all marked uploaded and which
    /// `fully_sealed` confirms has nothing left to seal.
    pub fn delete_done(&mut self, fully_sealed: impl Fn(&str) -> Result<bool>) -> Result<()> {
        if !self.delete || self.store.is_none() {
            return Ok(());
        }
        for mk in self.spool.list_out_minutes()? {
            let archives = self.spool.list_archives(&mk)?;
            let all_done = !archives.is_empty() && archives.iter().all(|(_, d)| *d);
            if all_done && fully_sealed(&mk)? {
                self.spool.delete_minute(&mk)?;
                self.stats.deleted_minutes += 1;
                info!(minute = %mk, "deleted after confirmed upload");
            }
        }
        Ok(())
    }

    /// Minutes of sealed-but-unuploaded output.
    pub fn backlog_minutes(&self) -> Result<u64> {
        let mut n = 0;
        for mk in self.spool.list_out_minutes()? {
            if self.spool.list_archives(&mk)?.iter().any(|(_, d)| !d) {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Three or more consecutive failures on any key.
    pub fn failing(&self) -> bool {
        self.backoff.max_consecutive_failures() >= 3
    }
}

/// The archives of one minute that may be uploaded now: every pending one,
/// except `last` while any other is still pending.
fn due(archives: &[(String, bool)], last: Option<&str>) -> Vec<String> {
    let others_pending = archives
        .iter()
        .any(|(ch, done)| !done && Some(ch.as_str()) != last);
    let held = |ch: &str| others_pending && Some(ch) == last;
    archives
        .iter()
        .filter(|(ch, done)| !done && !held(ch))
        .map(|(ch, _)| ch.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minute(a: &[(&str, bool)]) -> Vec<(String, bool)> {
        a.iter().map(|(c, d)| (c.to_string(), *d)).collect()
    }

    #[test]
    fn the_last_archive_waits_for_the_rest_of_its_minute() {
        let m = minute(&[("_up", false), ("a", true), ("b", false)]);
        assert_eq!(due(&m, Some("_up")), ["b"]);
        assert_eq!(due(&m, None), ["_up", "b"]);

        let m = minute(&[("_up", false), ("a", true), ("b", true)]);
        assert_eq!(due(&m, Some("_up")), ["_up"]);

        // A minute with nothing but the marker.
        assert_eq!(due(&minute(&[("_up", false)]), Some("_up")), ["_up"]);
        assert!(due(&minute(&[("_up", true), ("a", true)]), Some("_up")).is_empty());
    }
}
