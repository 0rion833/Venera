//! Per-provider API keys, sealed in the encrypted vault.
//!
//! Resolution order: environment override, then the unlocked vault. The vault is opened
//! with a master password (Argon2id) in the TUI; CLI callers can set
//! `VENERA_VAULT_PASSWORD`. No keyring or plaintext files are used for new keys.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use secrecy::SecretString;

use crate::provider::registry;
use crate::security::vault::{Vault, VaultError};

static VAULT: OnceLock<Mutex<Option<Vault>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Vault>> {
    VAULT.get_or_init(|| Mutex::new(None))
}

/// Where a resolved key came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOrigin {
    /// Environment variable, e.g. `AITUNNEL_API_KEY`.
    Env(String),
    /// Encrypted vault.
    Vault,
}

impl KeyOrigin {
    /// Short label safe to print.
    pub fn label(&self) -> String {
        match self {
            Self::Env(name) => format!("env {name}"),
            Self::Vault => "vault".to_owned(),
        }
    }
}

/// A key together with its origin.
pub struct ResolvedKey {
    /// The secret itself.
    pub secret: SecretString,
    /// Where it was found.
    pub origin: KeyOrigin,
}

/// What `/api` shows for one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStatus {
    /// No key in the environment or the vault.
    Missing,
    /// A key is present; only the masked prefix is exposed.
    Present { masked: String, origin: KeyOrigin },
    /// The vault is locked or unreadable.
    Error(String),
}

/// Key storage errors. Messages never include the key.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("API key is empty")]
    Empty,
    #[error("the key vault is locked; enter the master password")]
    Locked,
    #[error(transparent)]
    Vault(#[from] VaultError),
}

/// Default vault path: `<data-local>/venera/vault.json`.
pub fn vault_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VENERA_VAULT_PATH") {
        return Some(PathBuf::from(path));
    }
    directories::BaseDirs::new().map(|dirs| dirs.data_local_dir().join("venera").join("vault.json"))
}

/// True when a vault file exists.
pub fn vault_exists() -> bool {
    vault_path().is_some_and(|path| path.is_file())
}

/// True when the vault is open in this process.
pub fn is_unlocked() -> bool {
    slot().lock().map(|vault| vault.is_some()).unwrap_or(false)
}

/// Creates the vault and imports keys left by the keyring/file implementation.
pub fn create_vault(password: &str) -> Result<(), KeyError> {
    let path = vault_path().ok_or_else(|| {
        KeyError::Vault(VaultError::Io {
            path: PathBuf::from("<no data directory>"),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no data directory is available",
            ),
        })
    })?;
    let mut vault = Vault::create(path, password)?;
    migrate_legacy(&mut vault)?;
    *slot().lock().expect("vault mutex") = Some(vault);
    Ok(())
}

/// Opens the vault with the master password.
pub fn unlock(password: &str) -> Result<(), KeyError> {
    if is_unlocked() {
        return Ok(());
    }
    let path = vault_path().ok_or_else(|| {
        KeyError::Vault(VaultError::Io {
            path: PathBuf::from("<no data directory>"),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no data directory is available",
            ),
        })
    })?;
    let vault = Vault::open(path, password)?;
    *slot().lock().expect("vault mutex") = Some(vault);
    Ok(())
}

/// Unlocks from `VENERA_VAULT_PASSWORD` when it is set; non-interactive use.
pub fn unlock_from_env() -> Result<bool, KeyError> {
    if is_unlocked() {
        return Ok(true);
    }
    if !vault_exists() {
        return Ok(false);
    }
    let Some(password) = std::env::var_os("VENERA_VAULT_PASSWORD") else {
        return Ok(false);
    };
    let password = password.to_string_lossy();
    unlock(&password)?;
    Ok(true)
}

/// Deletes the vault: every API key becomes unrecoverable.
pub fn reset_vault() -> Result<(), KeyError> {
    let Some(path) = vault_path() else {
        return Ok(());
    };
    Vault::reset(&path)?;
    *slot().lock().expect("vault mutex") = None;
    cleanup_legacy();
    Ok(())
}

/// Resolves the key for `provider_id`: env, then vault.
pub fn resolve(provider_id: &str) -> Result<Option<ResolvedKey>, KeyError> {
    let Some(provider) = registry::get(provider_id) else {
        return Ok(None);
    };

    if let Ok(value) = std::env::var(provider.key_env) {
        let value = value.trim();
        if !value.is_empty() {
            return Ok(Some(ResolvedKey {
                secret: SecretString::from(value.to_owned()),
                origin: KeyOrigin::Env(provider.key_env.to_owned()),
            }));
        }
    }

    if !vault_exists() {
        return Ok(None);
    }
    let guard = slot()
        .lock()
        .map_err(|_| KeyError::Vault(VaultError::Corrupt("vault mutex poisoned")))?;
    let Some(vault) = guard.as_ref() else {
        return Err(KeyError::Locked);
    };
    Ok(vault.get(provider_id).map(|secret| ResolvedKey {
        secret,
        origin: KeyOrigin::Vault,
    }))
}

/// Stores a key in the unlocked vault.
pub fn store(provider_id: &str, key: &str) -> Result<KeyOrigin, KeyError> {
    let key = key.trim();
    if key.is_empty() {
        return Err(KeyError::Empty);
    }

    let mut guard = slot()
        .lock()
        .map_err(|_| KeyError::Vault(VaultError::Corrupt("vault mutex poisoned")))?;
    let Some(vault) = guard.as_mut() else {
        return Err(KeyError::Locked);
    };
    vault.set(provider_id, key)?;
    Ok(KeyOrigin::Vault)
}

/// Masked key and its origin, for lists. Never returns the full secret.
pub fn status(provider_id: &str) -> KeyStatus {
    match resolve(provider_id) {
        Ok(Some(resolved)) => KeyStatus::Present {
            masked: mask(&resolved.secret),
            origin: resolved.origin,
        },
        Ok(None) => KeyStatus::Missing,
        Err(err) => KeyStatus::Error(err.to_string()),
    }
}

/// `sk-490…` — enough to recognize a key, not enough to use it.
pub fn mask(secret: &SecretString) -> String {
    use secrecy::ExposeSecret as _;

    let key = secret.expose_secret();
    let count = key.chars().count();
    let take = if count > 6 { 6 } else { 2 };
    let mut visible: String = key.chars().take(take).collect();
    visible.push('…');
    visible
}

fn legacy_key_path(provider_id: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("VENERA_KEYS_DIR") {
        return Some(PathBuf::from(dir).join(provider_id));
    }
    directories::BaseDirs::new().map(|dirs| {
        dirs.data_local_dir()
            .join("venera")
            .join("keys")
            .join(provider_id)
    })
}

fn legacy_keyring_entry(provider_id: &str) -> Option<keyring::Entry> {
    keyring::Entry::new("venera", &format!("provider:{provider_id}")).ok()
}

fn migrate_legacy(vault: &mut Vault) -> Result<(), KeyError> {
    for provider in registry::PROVIDERS {
        if vault.get(provider.id).is_some() {
            continue;
        }

        let mut key = legacy_keyring_entry(provider.id)
            .and_then(|entry| entry.get_password().ok())
            .filter(|value| !value.trim().is_empty());

        if key.is_none()
            && let Some(path) = legacy_key_path(provider.id)
            && let Ok(value) = std::fs::read_to_string(&path)
        {
            let value = value.trim().to_owned();
            if !value.is_empty() {
                key = Some(value);
            }
        }

        if let Some(key) = key {
            vault.set(provider.id, &key)?;
            cleanup_one_legacy(provider.id);
        }
    }
    Ok(())
}

fn cleanup_legacy() {
    for provider in registry::PROVIDERS {
        cleanup_one_legacy(provider.id);
    }
}

fn cleanup_one_legacy(provider_id: &str) {
    if let Some(entry) = legacy_keyring_entry(provider_id) {
        let _ = entry.delete_credential();
    }
    if let Some(path) = legacy_key_path(provider_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Legacy key file directory, kept for tests and migration.
#[allow(dead_code)]
fn legacy_dir() -> Option<PathBuf> {
    legacy_key_path("probe").and_then(|path| path.parent().map(Path::to_path_buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masking_keeps_only_a_recognizable_prefix() {
        assert_eq!(mask(&SecretString::from("sk-490abcdefgh")), "sk-490…");
        assert_eq!(mask(&SecretString::from("short")), "sh…");
        assert_eq!(mask(&SecretString::from("abc")), "ab…");
    }
}
