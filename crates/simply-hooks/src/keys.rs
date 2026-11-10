//! Where an app keeps a linked device's key ([`crate::device`]): never in
//! a file or a settings store, but in the platform's keychain. [`Keys`]
//! is the seam: [`Keychain`] on macOS (the `keychain` feature), a stub that
//! says so elsewhere, and [`MemoryKeys`] for tests and for an app that keeps
//! nothing.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::Secret;

/// A store of keys, by account (a device id).
pub trait Keys: Send + Sync {
    /// The key kept for `account` (`Ok(None)`: none).
    fn get(&self, account: &str) -> Result<Option<Secret>, String>;
    /// Keep `key` for `account`, replacing one kept before.
    fn set(&self, account: &str, key: &Secret) -> Result<(), String>;
    /// Forget `account`'s key; nothing kept is no error.
    fn delete(&self, account: &str);
}

/// Keys in memory, gone with the process.
#[derive(Default)]
pub struct MemoryKeys(Mutex<HashMap<String, Secret>>);

impl Keys for MemoryKeys {
    fn get(&self, account: &str) -> Result<Option<Secret>, String> {
        Ok(self.0.lock().unwrap_or_else(|p| p.into_inner()).get(account).cloned())
    }
    fn set(&self, account: &str, key: &Secret) -> Result<(), String> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).insert(account.to_owned(), key.clone());
        Ok(())
    }
    fn delete(&self, account: &str) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).remove(account);
    }
}

/// The macOS keychain: a generic password per account under `service`
/// (`com.example.app.devices`). Without the `keychain` feature, or off
/// macOS, it keeps nothing and says why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keychain {
    pub service: String,
}

impl Keychain {
    pub fn new(service: impl Into<String>) -> Keychain {
        Keychain { service: service.into() }
    }
}

/// Why [`Keychain`] keeps nothing in this build.
#[cfg_attr(all(feature = "keychain", target_os = "macos"), allow(dead_code))]
const NO_KEYCHAIN: &str = "keeping a key needs the macOS keychain";

/// `errSecItemNotFound`.
#[cfg(all(feature = "keychain", target_os = "macos"))]
const ITEM_NOT_FOUND: i32 = -25300;

#[cfg(all(feature = "keychain", target_os = "macos"))]
impl Keys for Keychain {
    fn get(&self, account: &str) -> Result<Option<Secret>, String> {
        match security_framework::passwords::get_generic_password(&self.service, account) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(|k| Some(Secret::new(k)))
                .map_err(|_| "the keychain's key is not text".to_owned()),
            Err(e) if e.code() == ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(format!("the keychain refused: {e}")),
        }
    }
    fn set(&self, account: &str, key: &Secret) -> Result<(), String> {
        security_framework::passwords::set_generic_password(
            &self.service,
            account,
            key.expose().as_bytes(),
        )
        .map_err(|e| format!("the keychain refused to keep the key: {e}"))
    }
    fn delete(&self, account: &str) {
        let _ = security_framework::passwords::delete_generic_password(&self.service, account);
    }
}

#[cfg(not(all(feature = "keychain", target_os = "macos")))]
impl Keys for Keychain {
    fn get(&self, _: &str) -> Result<Option<Secret>, String> {
        Ok(None)
    }
    fn set(&self, _: &str, _: &Secret) -> Result<(), String> {
        Err(NO_KEYCHAIN.into())
    }
    fn delete(&self, _: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_keeps_and_forgets() {
        let k = MemoryKeys::default();
        assert_eq!(k.get("d").unwrap(), None);
        k.set("d", &Secret::new("key")).unwrap();
        assert_eq!(k.get("d").unwrap().as_ref().map(Secret::expose), Some("key"));
        k.delete("d");
        k.delete("d");
        assert_eq!(k.get("d").unwrap(), None);
    }
}
