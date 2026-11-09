//! What the app keeps: one file, `lumina.toml`, readable by the member only.
//!
//! It holds the endpoint that issued the token, the device id, the token, the
//! member Lumina linked it to, and the consent switches. Nothing else. The
//! token changes on every connect, so every write goes to a temporary file
//! created with mode 0600 and is renamed over the old one: a crash leaves
//! either the old token or the new one, never half of either. On Unix the
//! mode is set at creation, so the token is never readable by anyone else,
//! not even for a moment.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::wire::ConsentReport;

/// The file's name inside the directory the app gives the relay.
pub const FILE_NAME: &str = "lumina.toml";

/// The switches. Everything is off until the member turns it on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consent {
    /// "Let Lumina run diagnostics": the standard `lumina_*` tools and the
    /// app's own tools it declared read-only.
    #[serde(default)]
    pub read_only: bool,
    /// One switch per action class the app declared (`session`: "Let Lumina
    /// open and join jam sessions"). A class missing here is off.
    #[serde(default)]
    pub actions: BTreeMap<String, bool>,
}

impl Consent {
    /// Whether the switches allow a tool of `class`.
    pub fn allows(&self, class: &Class) -> bool {
        match class {
            Class::ReadOnly => self.read_only,
            Class::Action(name) => self.actions.get(name).copied().unwrap_or(false),
        }
    }

    pub(crate) fn report(&self) -> ConsentReport {
        ConsentReport { read_only: self.read_only, actions: self.actions.clone() }
    }
}

/// What a tool may do, as the app declares it. A tool the app did not
/// declare has no class: it is never listed and never called.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Class {
    /// Reads only. Covered by the diagnostics switch.
    ReadOnly,
    /// Changes something, under the named action class's own switch.
    Action(String),
}

impl Class {
    /// `read_only`, or the action class's name.
    pub fn name(&self) -> &str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Action(name) => name,
        }
    }
}

/// The file's contents.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stored {
    /// The endpoint that issued the token; later connects go there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Whose account the device is linked to, as Lumina named the member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_to: Option<String>,
    #[serde(default)]
    pub consent: Consent,
}

impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stored")
            .field("endpoint", &self.endpoint)
            .field("device_id", &self.device_id)
            .field("token", &self.token.as_ref().map(|_| "[redacted]"))
            .field("linked_to", &self.linked_to)
            .field("consent", &self.consent)
            .finish()
    }
}

impl Stored {
    /// The device id and token, when the app is linked.
    pub fn credentials(&self) -> Option<(&str, &str)> {
        Some((self.device_id.as_deref()?, self.token.as_deref()?))
    }

    /// Forget the link, keep the switches.
    pub fn forget_link(&mut self) {
        self.endpoint = None;
        self.device_id = None;
        self.token = None;
        self.linked_to = None;
    }
}

/// The file at `dir/lumina.toml`.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self { path: dir.as_ref().join(FILE_NAME) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The stored state; a missing file is the default (unlinked, all off).
    pub fn load(&self) -> io::Result<Stored> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => toml::from_str(&text).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", self.path.display()))
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Stored::default()),
            Err(e) => Err(e),
        }
    }

    /// Replace the file atomically, mode 0600.
    pub fn save(&self, stored: &Stored) -> io::Result<()> {
        let text = toml::to_string(stored).map_err(|e| io::Error::other(e.to_string()))?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("toml.tmp");
        {
            let mut file = private_file(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(unix)]
fn private_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // A leftover temporary file keeps its old mode; make sure.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(path)
}
