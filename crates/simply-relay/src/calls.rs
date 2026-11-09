//! The member's own record of what Lumina asked: `lumina-calls.log`.
//!
//! Every `tools/call`, allowed or refused, is one JSON line: time, tool,
//! class, verdict and the purpose Lumina gave in the request's `_meta`.
//! Argument values are never written (a join ticket is an argument). The file
//! rolls over to `lumina-calls.log.1` past 1 MiB, so it never grows without
//! bound and the last few thousand calls are always there.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The log's file name inside the relay's log directory.
pub const FILE_NAME: &str = "lumina-calls.log";

const ROLL_AT: u64 = 1 << 20;

/// Whether the call reached the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The switches allowed it; the app ran it.
    Allowed,
    /// Refused before the app saw it.
    Refused,
}

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRecord {
    /// UTC, RFC 3339, to the second.
    pub at: String,
    pub tool: String,
    /// `read_only`, an action class's name, or `none` for an undeclared tool.
    pub class: String,
    pub verdict: Verdict,
    /// What Lumina said the call was for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

/// Appends to and reads back the log.
#[derive(Debug)]
pub struct CallLog {
    path: PathBuf,
    write: Mutex<()>,
}

impl CallLog {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self { path: dir.as_ref().join(FILE_NAME), write: Mutex::new(()) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record. A failure is logged and otherwise ignored: a full
    /// disk must not turn an allowed call into a failed one.
    pub fn append(&self, record: &CallRecord) {
        if let Err(e) = self.try_append(record) {
            tracing::warn!("relay: could not write {}: {e}", self.path.display());
        }
    }

    fn try_append(&self, record: &CallRecord) -> io::Result<()> {
        let _guard = self.write.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if std::fs::metadata(&self.path).map(|m| m.len() >= ROLL_AT).unwrap_or(false) {
            std::fs::rename(&self.path, self.path.with_extension("log.1"))?;
        }
        let mut line = serde_json::to_string(record).map_err(io::Error::other)?;
        line.push('\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(line.as_bytes())
    }

    /// The last `n` records, oldest first.
    pub fn recent(&self, n: usize) -> Vec<CallRecord> {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        let mut records: Vec<CallRecord> =
            text.lines().rev().filter_map(|l| serde_json::from_str(l).ok()).take(n).collect();
        records.reverse();
        records
    }
}

/// Now, as `2026-11-09T08:19:06Z`.
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339(secs)
}

/// `secs` after the Unix epoch, as RFC 3339 UTC.
pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_matches_known_instants() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_793_866_746), "2026-11-05T08:19:06Z");
    }
}
