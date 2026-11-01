//! The on-disk spool.
//!
//! There is no database. Every pipeline state is a location on disk and every
//! transition is an atomic rename on the same filesystem, so a crash anywhere
//! resumes from a directory walk. See [`layout`] for the paths.

pub mod driver;
pub mod layout;
pub mod time;
pub mod zipw;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use driver::Driver;
pub use layout::Spool;

/// Write `bytes` to `path` via a `.tmp` sibling and an atomic rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path(path);
    {
        let mut f = std::fs::File::create(&tmp)?;
        io::Write::write_all(&mut f, bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

pub fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Exponential backoff with jitter, keyed by string, held in memory only.
/// Restarting the process resets it, which is what we want after a restart.
pub struct Backoff {
    base: Duration,
    cap: Duration,
    state: HashMap<String, (u32, Instant)>,
}

impl Backoff {
    pub fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            state: HashMap::new(),
        }
    }

    pub fn ready(&self, key: &str, now: Instant) -> bool {
        match self.state.get(key) {
            None => true,
            Some((_, next)) => now >= *next,
        }
    }

    /// Record a failure; returns the number of consecutive failures. `floor`
    /// is a minimum wait, for answers that will not change soon (a key that
    /// is not allowed to write somewhere).
    pub fn failed(&mut self, key: &str, now: Instant, floor: Duration) -> u32 {
        let entry = self.state.entry(key.to_string()).or_insert((0, now));
        entry.0 += 1;
        let exp = self.base.as_millis() as u64 * 2u64.saturating_pow(entry.0.min(16));
        let capped = exp.min(self.cap.as_millis() as u64).max(floor.as_millis() as u64);
        // Deterministic jitter from the key so we need no RNG dependency.
        let jitter = (fxhash(key) % 1000) * capped / 4000; // up to +25%
        entry.1 = now + Duration::from_millis(capped + jitter);
        entry.0
    }

    pub fn clear(&mut self, key: &str) {
        self.state.remove(key);
    }

    pub fn max_consecutive_failures(&self) -> u32 {
        self.state.values().map(|s| s.0).max().unwrap_or(0)
    }
}

fn fxhash(s: &str) -> u64 {
    s.bytes()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_clears() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(300));
        let t0 = Instant::now();
        assert!(b.ready("k", t0));
        b.failed("k", t0, Duration::ZERO);
        assert!(!b.ready("k", t0));
        assert!(b.ready("k", t0 + Duration::from_secs(3)));
        for _ in 0..20 {
            b.failed("k", t0, Duration::ZERO);
        }
        assert!(!b.ready("k", t0 + Duration::from_secs(299)));
        assert!(b.ready("k", t0 + Duration::from_secs(400)));
        b.clear("k");
        assert!(b.ready("k", t0));
    }

    #[test]
    fn backoff_floor() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(300));
        let t0 = Instant::now();
        b.failed("k", t0, Duration::from_secs(300));
        assert!(!b.ready("k", t0 + Duration::from_secs(299)));
    }

    #[test]
    fn atomic_write_replaces() {
        let dir = std::env::temp_dir().join(format!("read5-spool-test-{}", std::process::id()));
        let p = dir.join("a").join("b.txt");
        atomic_write(&p, b"one").unwrap();
        atomic_write(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert!(!tmp_path(&p).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
