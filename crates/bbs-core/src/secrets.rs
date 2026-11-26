//! Zeroized secret token handling and OS keyring storage contracts.
//!
//! Security invariants:
//! - [`SecretToken`] wraps [`Zeroizing<String>`] so memory is scrubbed on drop.
//! - [`SecretToken`] intentionally does NOT implement `Serialize` or `Display`, and its
//!   `Debug` implementation always emits `SecretToken([REDACTED])`.
//! - Importing an existing `discordo` keyring entry (`service="discordo"`, `user="token"`) is
//!   strictly **opt-in and read-only** in RAM — it never mutates `discordo`'s entry and never
//!   writes secrets to disk.

use std::collections::HashMap;
use std::fmt;
use std::sync::RwLock;

use zeroize::Zeroizing;

use crate::error::{CoreError, CoreResult};

/// Primary OS keyring service name for `bbscord`.
pub const BBSCORD_KEYRING_SERVICE: &str = "bbscord";
/// Primary OS keyring username/account for `bbscord`.
pub const BBSCORD_KEYRING_USER: &str = "token";

/// External `discordo` OS keyring service name for opt-in read-only RAM import.
pub const DISCORDO_KEYRING_SERVICE: &str = "discordo";
/// External `discordo` OS keyring username/account for opt-in read-only RAM import.
pub const DISCORDO_KEYRING_USER: &str = "token";

/// Provenance of an in-memory authentication token (safe to log; never contains the secret).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenOrigin {
    /// Loaded from `bbscord`'s own OS keyring entry (`service="bbscord"`, `user="token"`).
    BbscordKeyring,
    /// Opt-in read-only import from `discordo`'s OS keyring entry held purely in RAM.
    DiscordoReadOnlyRamImport,
    /// Supplied via process environment variable in RAM only.
    EnvironmentVariable,
    /// Entered interactively by the user in RAM only.
    InteractivePromptRamOnly,
}

/// Zeroized authentication token with redacted `Debug` output.
///
/// Intentionally omits `Display` and `Serialize` so tokens cannot accidentally leak into
/// format strings, tracing spans, JSON payloads, or TOML config files.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretToken(Zeroizing<String>);

impl SecretToken {
    /// Wrap a raw secret string after trimming surrounding ASCII whitespace.
    ///
    /// Returns [`CoreError::AuthRequired`] if the resulting token is empty.
    pub fn new(raw: impl Into<String>) -> CoreResult<Self> {
        let mut owned = Zeroizing::new(raw.into());
        let trimmed = owned.trim();
        if trimmed.is_empty() {
            return Err(CoreError::AuthRequired);
        }
        if trimmed.len() == owned.len() {
            Ok(Self(owned))
        } else {
            let clean = Zeroizing::new(trimmed.to_owned());
            // Ensure intermediate buffer is cleared immediately.
            owned.clear();
            Ok(Self(clean))
        }
    }

    /// Explicitly expose the underlying secret `&str` at audited authorization boundaries
    /// (e.g. HTTP `Authorization` header or Gateway `IDENTIFY` payload).
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        self.0.as_str()
    }

    /// Character length in bytes (safe metadata for validation).
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns `true` if the token is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken([REDACTED])")
    }
}

/// Abstract credential store enabling both OS keyrings and offline in-memory test doubles.
pub trait KeyringStore: Send + Sync {
    /// Read a secret from `(service, user)` returning `Ok(None)` when absent.
    fn get_secret(&self, service: &str, user: &str) -> CoreResult<Option<SecretToken>>;

    /// Persist a secret to `(service, user)`.
    fn set_secret(&self, service: &str, user: &str, token: &SecretToken) -> CoreResult<()>;

    /// Delete the secret at `(service, user)`. Missing entries are treated as `Ok(())`.
    fn delete_secret(&self, service: &str, user: &str) -> CoreResult<()>;

    /// Load `bbscord`'s own stored token (`service="bbscord"`, `user="token"`).
    fn load_bbscord_token(&self) -> CoreResult<Option<SecretToken>> {
        self.get_secret(BBSCORD_KEYRING_SERVICE, BBSCORD_KEYRING_USER)
    }

    /// Save a token into `bbscord`'s own keyring entry (`service="bbscord"`, `user="token"`).
    fn save_bbscord_token(&self, token: &SecretToken) -> CoreResult<()> {
        self.set_secret(BBSCORD_KEYRING_SERVICE, BBSCORD_KEYRING_USER, token)
    }

    /// Remove `bbscord`'s stored token from the keyring.
    fn delete_bbscord_token(&self) -> CoreResult<()> {
        self.delete_secret(BBSCORD_KEYRING_SERVICE, BBSCORD_KEYRING_USER)
    }

    /// Opt-in, strictly read-only import of an existing `discordo` keyring token into RAM.
    ///
    /// This method NEVER writes to disk, NEVER modifies `discordo`'s keyring entry, and
    /// NEVER copies the token into `bbscord`'s keyring automatically.
    fn import_discordo_token_ram_only(&self) -> CoreResult<Option<SecretToken>> {
        self.get_secret(DISCORDO_KEYRING_SERVICE, DISCORDO_KEYRING_USER)
    }
}

/// Production OS keyring store backed by `keyring` 4.2 (Secret Service / Keychain / Credential Manager).
#[derive(Debug, Default, Clone, Copy)]
pub struct OsKeyringStore;

impl OsKeyringStore {
    /// Create a handle to the platform OS credential store.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn map_keyring_error(err: keyring::Error) -> CoreError {
        // Deliberately sanitize all keyring errors so raw secret bytes from `BadEncoding`
        // or `BadDataFormat` can NEVER leak into logs or error banners.
        match err {
            keyring::Error::NoEntry => CoreError::NotFound("keyring entry not found".to_string()),
            keyring::Error::NoDefaultStore => {
                CoreError::SecretStore("no default OS credential store available".to_string())
            }
            keyring::Error::NoStorageAccess(_) => {
                CoreError::SecretStore("OS credential store is locked or inaccessible".to_string())
            }
            keyring::Error::BadEncoding(_) => {
                CoreError::SecretStore("stored credential is not valid UTF-8".to_string())
            }
            keyring::Error::BadDataFormat(_, _) => {
                CoreError::SecretStore("stored credential data is malformed".to_string())
            }
            keyring::Error::BadStoreFormat(_) => {
                CoreError::SecretStore("OS credential store format error".to_string())
            }
            keyring::Error::TooLong(_, _) => {
                CoreError::SecretStore("credential exceeds OS keyring length limit".to_string())
            }
            keyring::Error::Invalid(_, _) => {
                CoreError::SecretStore("invalid keyring service or user identifier".to_string())
            }
            keyring::Error::Ambiguous(_) => {
                CoreError::SecretStore("ambiguous keyring entries matched query".to_string())
            }
            keyring::Error::NotSupportedByStore(_) => {
                CoreError::Unsupported("operation not supported by OS keyring".to_string())
            }
            keyring::Error::PlatformFailure(_) => {
                CoreError::SecretStore("platform keyring backend failure".to_string())
            }
            _ => CoreError::SecretStore("unexpected OS keyring error".to_string()),
        }
    }
}

impl KeyringStore for OsKeyringStore {
    fn get_secret(&self, service: &str, user: &str) -> CoreResult<Option<SecretToken>> {
        let entry = keyring::Entry::new(service, user).map_err(Self::map_keyring_error)?;
        match entry.get_password() {
            Ok(raw) => {
                let raw = Zeroizing::new(raw);
                if raw.trim().is_empty() {
                    return Ok(None);
                }
                SecretToken::new(raw.as_str()).map(Some)
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(Self::map_keyring_error(err)),
        }
    }

    fn set_secret(&self, service: &str, user: &str, token: &SecretToken) -> CoreResult<()> {
        let entry = keyring::Entry::new(service, user).map_err(Self::map_keyring_error)?;
        entry
            .set_password(token.expose_secret())
            .map_err(Self::map_keyring_error)
    }

    fn delete_secret(&self, service: &str, user: &str) -> CoreResult<()> {
        let entry = keyring::Entry::new(service, user).map_err(Self::map_keyring_error)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(Self::map_keyring_error(err)),
        }
    }
}

/// Pure in-memory [`KeyringStore`] for unit tests and offline `--mock-demo` sessions.
#[derive(Debug, Default)]
pub struct MemoryKeyringStore {
    entries: RwLock<HashMap<(String, String), SecretToken>>,
}

impl MemoryKeyringStore {
    /// Create an empty in-memory credential store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl KeyringStore for MemoryKeyringStore {
    fn get_secret(&self, service: &str, user: &str) -> CoreResult<Option<SecretToken>> {
        let guard = self
            .entries
            .read()
            .map_err(|_| CoreError::Internal("memory keyring lock poisoned".to_string()))?;
        Ok(guard.get(&(service.to_owned(), user.to_owned())).cloned())
    }

    fn set_secret(&self, service: &str, user: &str, token: &SecretToken) -> CoreResult<()> {
        let mut guard = self
            .entries
            .write()
            .map_err(|_| CoreError::Internal("memory keyring lock poisoned".to_string()))?;
        guard.insert((service.to_owned(), user.to_owned()), token.clone());
        Ok(())
    }

    fn delete_secret(&self, service: &str, user: &str) -> CoreResult<()> {
        let mut guard = self
            .entries
            .write()
            .map_err(|_| CoreError::Internal("memory keyring lock poisoned".to_string()))?;
        guard.remove(&(service.to_owned(), user.to_owned()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_token_debug_is_always_redacted() {
        let token = SecretToken::new("  unit-test-placeholder-credential  ").unwrap();
        assert_eq!(token.expose_secret(), "unit-test-placeholder-credential");
        let debug_str = format!("{token:?}");
        assert_eq!(debug_str, "SecretToken([REDACTED])");
        assert!(!debug_str.contains("placeholder"));
    }

    #[test]
    fn secret_token_rejects_empty_or_whitespace() {
        assert_eq!(SecretToken::new(""), Err(CoreError::AuthRequired));
        assert_eq!(SecretToken::new("   \n\t "), Err(CoreError::AuthRequired));
    }

    #[test]
    fn memory_keyring_bbscord_and_readonly_discordo_import() {
        let store = MemoryKeyringStore::new();
        assert_eq!(store.load_bbscord_token().unwrap(), None);
        assert_eq!(store.import_discordo_token_ram_only().unwrap(), None);

        // Seed only the external discordo slot in the in-memory double.
        let external = SecretToken::new("unit-test-discordo-slot").unwrap();
        store
            .set_secret(DISCORDO_KEYRING_SERVICE, DISCORDO_KEYRING_USER, &external)
            .unwrap();

        // Opt-in import reads purely into RAM and does NOT write to bbscord's slot.
        let imported = store.import_discordo_token_ram_only().unwrap().unwrap();
        assert_eq!(imported.expose_secret(), "unit-test-discordo-slot");
        assert_eq!(store.load_bbscord_token().unwrap(), None);

        // Explicit save writes only to bbscord's slot.
        let own = SecretToken::new("unit-test-bbscord-slot").unwrap();
        store.save_bbscord_token(&own).unwrap();
        assert_eq!(
            store.load_bbscord_token().unwrap().unwrap().expose_secret(),
            "unit-test-bbscord-slot"
        );
        // Deleting bbscord token leaves external discordo slot untouched.
        store.delete_bbscord_token().unwrap();
        assert_eq!(store.load_bbscord_token().unwrap(), None);
        assert_eq!(
            store
                .import_discordo_token_ram_only()
                .unwrap()
                .unwrap()
                .expose_secret(),
            "unit-test-discordo-slot"
        );
    }
}
