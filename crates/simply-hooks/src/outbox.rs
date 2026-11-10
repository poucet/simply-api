//! The outbox: every delivery not yet made, in one small JSON file in the
//! app's data directory, rewritten whole (to a temporary file, then renamed)
//! after every change, owner-only on Unix. A delivery leaves it when it is
//! made, refused for good, or older than the max age. Secrets are never
//! written here: an entry names its target by URL and is signed at send time.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file's format version.
const VERSION: u32 = 1;

/// One delivery: an event's body, to one target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    /// `X-Hook-Delivery`: the same on every attempt.
    pub id: String,
    pub event: String,
    pub url: String,
    /// The exact body, signed afresh on every attempt.
    pub body: String,
    /// When it was emitted, unix seconds.
    pub created: u64,
    pub attempts: u32,
    /// Not before this, unix milliseconds.
    pub next_ms: u64,
    /// The sequence it is in ([`crate::Hooks::emit_in`]): it waits for the
    /// earlier entries to its URL in the same sequence. `None`: in none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<String>,
    /// The event it delivers ([`crate::EventId`]): what
    /// [`crate::Hooks::deliveries`] finds it by, after a restart too.
    /// `None`: written before events had ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// Why the last attempt failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl Entry {
    /// What orders it: its URL and sequence (`None`: nothing does).
    pub fn chain(&self) -> Option<(&str, &str)> {
        self.seq.as_deref().map(|s| (self.url.as_str(), s))
    }
}

#[derive(Serialize, Deserialize)]
struct File {
    v: u32,
    entries: Vec<Entry>,
}

/// The entries, and where they are kept (`None`: in memory only).
#[derive(Debug, Default)]
pub(crate) struct Outbox {
    pub entries: Vec<Entry>,
    path: Option<PathBuf>,
}

impl Outbox {
    /// What `path` holds (nothing when absent or unreadable: a broken outbox
    /// costs its pending deliveries, never the app's start).
    pub fn open(path: Option<PathBuf>) -> Outbox {
        let entries = path.as_deref().map(read).unwrap_or_default();
        Outbox { entries, path }
    }

    /// Write it out; a failure is logged, the entries stay in memory.
    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Err(e) = write(path, &self.entries) {
            tracing::warn!("webhooks: could not write the outbox {}: {e}", path.display());
        }
    }

    /// Which entries may be tried: all but those waiting on an earlier
    /// entry of their sequence, one per index.
    pub fn ready(&self) -> Vec<bool> {
        let mut heads = std::collections::BTreeSet::new();
        self.entries.iter().map(|e| e.chain().is_none_or(|c| heads.insert(c))).collect()
    }

    /// The earliest time an entry that may be tried is due, unix ms.
    pub fn next_due(&self) -> Option<u64> {
        self.entries.iter().zip(self.ready()).filter(|(_, r)| *r).map(|(e, _)| e.next_ms).min()
    }
}

fn read(path: &Path) -> Vec<Entry> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return vec![],
        Err(e) => {
            tracing::warn!("webhooks: could not read the outbox {}: {e}", path.display());
            return vec![];
        }
    };
    match serde_json::from_str::<File>(&text) {
        Ok(f) if f.v == VERSION => f.entries,
        Ok(f) => {
            tracing::warn!(
                "webhooks: outbox {} is version {}: starting empty",
                path.display(),
                f.v
            );
            vec![]
        }
        Err(e) => {
            tracing::warn!(
                "webhooks: outbox {} is unreadable ({e}): starting empty",
                path.display()
            );
            vec![]
        }
    }
}

fn write(path: &Path, entries: &[Entry]) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_vec(&File { v: VERSION, entries: entries.to_vec() })?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let mut f = o.open(&tmp)?;
        std::io::Write::write_all(&mut f, &text)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, next_ms: u64) -> Entry {
        Entry {
            id: id.into(),
            event: "app.started".into(),
            url: "https://x/".into(),
            body: "{}".into(),
            created: 1,
            attempts: 0,
            next_ms,
            seq: None,
            event_id: None,
            last_error: None,
        }
    }

    #[test]
    fn it_survives_a_reopen_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/hooks-outbox.json");
        let mut o = Outbox::open(Some(path.clone()));
        assert!(o.entries.is_empty());
        o.entries = vec![entry("a", 30), entry("b", 20)];
        o.save();
        let again = Outbox::open(Some(path.clone()));
        assert_eq!(again.entries, o.entries);
        assert_eq!(again.next_due(), Some(20));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let old = r#"{"v":1,"entries":[{"id":"a","event":"e","url":"u","body":"{}","created":1,"attempts":0,"next_ms":5}]}"#;
        std::fs::write(&path, old).unwrap();
        assert_eq!(
            (
                Outbox::open(Some(path.clone())).entries[0].seq.clone(),
                Outbox::open(Some(path.clone())).entries[0].event_id.clone()
            ),
            (None, None),
            "an entry in no sequence reads as before"
        );
        std::fs::write(&path, "not json").unwrap();
        assert!(Outbox::open(Some(path)).entries.is_empty(), "broken: starts empty");
    }

    #[test]
    fn an_entry_waits_for_the_earlier_ones_of_its_sequence() {
        let seq = |id: &str, url: &str, s: Option<&str>, next_ms| Entry {
            url: url.into(),
            seq: s.map(str::to_owned),
            ..entry(id, next_ms)
        };
        let o = Outbox {
            entries: vec![
                seq("a", "https://x/", Some("s1"), 50),
                seq("b", "https://x/", Some("s1"), 0),
                seq("c", "https://x/", Some("s2"), 40),
                seq("d", "https://y/", Some("s1"), 30),
                seq("e", "https://x/", None, 60),
                seq("f", "https://x/", None, 70),
            ],
            path: None,
        };
        assert_eq!(o.ready(), [true, false, true, true, true, true]);
        assert_eq!(o.next_due(), Some(30), "b, due at 0, waits for a");
    }
}
