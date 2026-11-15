//! Encrypted API-key vault.
//!
//! A master password is stretched with Argon2id into a 32-byte key; the key map is
//! sealed with XChaCha20-Poly1305. Hashing alone cannot be reversed, so encryption is
//! what lets `/api` show a masked key later. The master password itself is never stored:
//! a failed AEAD tag means a wrong password (or a tampered file).
//!
//! Reset deletes the vault; without the master password the ciphertext is useless.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// Associated data binds the ciphertext to this file format.
const AAD: &[u8] = b"venera-vault-v1";

/// Argon2id cost: 32 MiB, 2 passes, one lane.
///
/// These numbers must never change: the parameters are not stored in the vault file,
/// so any change would silently stop every existing vault from decrypting.
const KDF_MEMORY_KIB: u32 = 32 * 1024;
const KDF_PASSES: u32 = 2;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;

/// The unlocked vault.
pub struct Vault {
    path: PathBuf,
    salt: [u8; SALT_LEN],
    key: Zeroizing<[u8; KEY_LEN]>,
    keys: BTreeMap<String, Zeroizing<String>>,
}

#[derive(Serialize, Deserialize)]
struct VaultFile {
    version: u8,
    kdf: String,
    salt: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Serialize, Deserialize)]
struct VaultBody {
    version: u8,
    keys: BTreeMap<String, String>,
}

impl Vault {
    /// Creates an empty vault at `path`.
    pub fn create(path: impl Into<PathBuf>, password: &str) -> Result<Self, VaultError> {
        let path = path.into();
        if path.exists() {
            return Err(VaultError::Exists(path));
        }

        let salt = random_salt()?;
        let key = derive(password, &salt)?;
        let vault = Self {
            path,
            salt,
            key: Zeroizing::new(key),
            keys: BTreeMap::new(),
        };
        vault.save()?;
        Ok(vault)
    }

    /// Opens and decrypts an existing vault.
    pub fn open(path: impl Into<PathBuf>, password: &str) -> Result<Self, VaultError> {
        let path = path.into();
        let text = std::fs::read_to_string(&path).map_err(|source| VaultError::Io {
            path: path.clone(),
            source,
        })?;
        let file: VaultFile = serde_json::from_str(&text).map_err(VaultError::Json)?;

        if file.version != 1 || file.kdf != "argon2id" {
            return Err(VaultError::Unsupported);
        }

        let salt: [u8; SALT_LEN] = decode(&file.salt, "salt")?
            .try_into()
            .map_err(|_| VaultError::Corrupt("salt length"))?;
        let nonce: [u8; NONCE_LEN] = decode(&file.nonce, "nonce")?
            .try_into()
            .map_err(|_| VaultError::Corrupt("nonce length"))?;
        let ciphertext = decode(&file.ciphertext, "ciphertext")?;
        let key = derive(password, &salt)?;
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&key));

        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: &ciphertext,
                        aad: AAD,
                    },
                )
                .map_err(|_| VaultError::WrongPassword)?,
        );

        let body: VaultBody = serde_json::from_slice(&plaintext).map_err(VaultError::Json)?;
        if body.version != 1 {
            return Err(VaultError::Unsupported);
        }

        let keys = body
            .keys
            .into_iter()
            .map(|(name, value)| (name, Zeroizing::new(value)))
            .collect();

        Ok(Self {
            path,
            salt,
            key: Zeroizing::new(key),
            keys,
        })
    }

    /// Stored key for a provider, as a secret.
    pub fn get(&self, provider: &str) -> Option<SecretString> {
        self.keys
            .get(provider)
            .map(|key| SecretString::from(key.as_str().to_owned()))
    }

    /// Provider ids present in the vault.
    pub fn providers(&self) -> Vec<&str> {
        self.keys.keys().map(String::as_str).collect()
    }

    /// Inserts or replaces a key and rewrites the vault.
    pub fn set(&mut self, provider: &str, key: &str) -> Result<(), VaultError> {
        let key = key.trim();
        if key.is_empty() {
            return Err(VaultError::EmptyKey);
        }
        self.keys
            .insert(provider.to_owned(), Zeroizing::new(key.to_owned()));
        self.save()
    }

    /// Removes a key and rewrites the vault.
    pub fn remove(&mut self, provider: &str) -> Result<(), VaultError> {
        self.keys.remove(provider);
        self.save()
    }

    /// Deletes the vault file; every key becomes unrecoverable.
    pub fn reset(path: &Path) -> Result<(), VaultError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(VaultError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// File the vault lives in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn save(&self) -> Result<(), VaultError> {
        let body = VaultBody {
            version: 1,
            keys: self
                .keys
                .iter()
                .map(|(name, value)| (name.clone(), value.as_str().to_owned()))
                .collect(),
        };
        let plaintext = Zeroizing::new(serde_json::to_vec(&body).map_err(VaultError::Json)?);

        let mut nonce = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce).map_err(|error| VaultError::Random(error.to_string()))?;

        let cipher = XChaCha20Poly1305::new(Key::from_slice(&*self.key));
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &plaintext,
                    aad: AAD,
                },
            )
            .map_err(|_| VaultError::Encrypt)?;

        let file = VaultFile {
            version: 1,
            kdf: "argon2id".to_owned(),
            salt: encode(&self.salt),
            nonce: encode(&nonce),
            ciphertext: encode(&ciphertext),
        };
        let text = serde_json::to_string_pretty(&file).map_err(VaultError::Json)?;

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| VaultError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        // Atomic replace: write a sibling temp file, sync it, then rename over the
        // vault. A crash mid-write leaves the old vault intact instead of a truncated
        // ciphertext, which would lock the user out of every stored key.
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let file_name = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "vault".to_owned());
        let temp_path = parent.join(format!(".{file_name}.tmp"));

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut temp = options.open(&temp_path).map_err(|source| VaultError::Io {
            path: temp_path.clone(),
            source,
        })?;
        temp.write_all(text.as_bytes())
            .and_then(|()| temp.sync_all())
            .map_err(|source| VaultError::Io {
                path: temp_path.clone(),
                source,
            })?;
        drop(temp);

        std::fs::rename(&temp_path, &self.path).map_err(|source| VaultError::Io {
            path: self.path.clone(),
            source,
        })?;

        #[cfg(unix)]
        {
            std::fs::File::open(parent)
                .and_then(|dir| dir.sync_all())
                .ok();
        }

        Ok(())
    }
}

fn derive(password: &str, salt: &[u8]) -> Result<[u8; KEY_LEN], VaultError> {
    let params = Params::new(KDF_MEMORY_KIB, KDF_PASSES, 1, Some(KEY_LEN))
        .map_err(|error| VaultError::Kdf(error.to_string()))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut key = [0u8; KEY_LEN];
    argon
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|error| VaultError::Kdf(error.to_string()))?;
    Ok(key)
}

fn random_salt() -> Result<[u8; SALT_LEN], VaultError> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::getrandom(&mut salt).map_err(|error| VaultError::Random(error.to_string()))?;
    Ok(salt)
}

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn decode(text: &str, field: &'static str) -> Result<Vec<u8>, VaultError> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|_| VaultError::Corrupt(field))
}

/// Vault failures. Messages never contain a key or a password.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("vault already exists at {0}")]
    Exists(PathBuf),
    #[error("vault I/O failed at {path}: {source}")]
    Io {
        /// File that failed.
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("vault file is not valid JSON: {0}")]
    Json(#[source] serde_json::Error),
    #[error("vault format is not supported")]
    Unsupported,
    #[error("vault is corrupt: {0}")]
    Corrupt(&'static str),
    #[error("wrong master password or damaged vault")]
    WrongPassword,
    #[error("Argon2 failed: {0}")]
    Kdf(String),
    #[error("cannot encrypt the vault")]
    Encrypt,
    #[error("system random generator failed: {0}")]
    Random(String),
    #[error("API key is empty")]
    EmptyKey,
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret as _;

    fn temp_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("vault.json");
        (dir, path)
    }

    #[test]
    fn a_vault_roundtrips_with_its_master_password() {
        let (_dir, path) = temp_path();
        let mut vault = Vault::create(&path, "correct horse battery staple").expect("create");
        vault.set("aitunnel", "sk-aitunnel-123").expect("set key");
        drop(vault);

        let vault = Vault::open(&path, "correct horse battery staple").expect("open");
        assert_eq!(
            vault.get("aitunnel").expect("key").expose_secret(),
            "sk-aitunnel-123"
        );
        assert!(
            !std::fs::read_to_string(&path)
                .expect("read")
                .contains("sk-aitunnel-123")
        );
    }

    #[test]
    fn a_wrong_password_does_not_decrypt() {
        let (_dir, path) = temp_path();
        let mut vault = Vault::create(&path, "one password").expect("create");
        vault.set("proxyapi", "sk-test").expect("set");
        drop(vault);

        assert!(matches!(
            Vault::open(&path, "another password"),
            Err(VaultError::WrongPassword)
        ));
    }

    #[test]
    fn a_tampered_ciphertext_is_rejected() {
        let (_dir, path) = temp_path();
        let mut vault = Vault::create(&path, "password").expect("create");
        vault.set("proxyapi", "sk-test").expect("set");
        drop(vault);

        let mut file: VaultFile =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        let mut bytes = decode(&file.ciphertext, "ciphertext").expect("decode");
        bytes[0] ^= 1;
        file.ciphertext = encode(&bytes);
        std::fs::write(&path, serde_json::to_string(&file).expect("json")).expect("write");

        assert!(matches!(
            Vault::open(&path, "password"),
            Err(VaultError::WrongPassword)
        ));
    }

    #[test]
    fn reset_removes_the_file_and_its_keys() {
        let (_dir, path) = temp_path();
        Vault::create(&path, "password").expect("create");
        assert!(path.exists());

        Vault::reset(&path).expect("reset");
        assert!(!path.exists());
        assert!(Vault::reset(&path).is_ok(), "reset is idempotent");
    }

    #[test]
    fn a_stale_temp_file_does_not_break_the_vault() {
        let (_dir, path) = temp_path();
        let mut vault = Vault::create(&path, "password").expect("create");
        vault.set("proxyapi", "sk-first").expect("set");

        // Simulate a crashed save: a leftover sibling temp file from an aborted write.
        let parent = path.parent().expect("parent");
        let temp = parent.join(".vault.json.tmp");
        std::fs::write(&temp, "partial garbage").expect("stale temp");
        drop(vault);

        // The vault itself must still decrypt; the stale temp is ignored on open.
        let mut reopened = Vault::open(&path, "password").expect("open");
        assert_eq!(
            reopened.get("proxyapi").expect("key").expose_secret(),
            "sk-first"
        );

        // The next save replaces the stale temp and then renames it into place.
        reopened.set("proxyapi", "sk-second").expect("set");
        drop(reopened);
        let again = Vault::open(&path, "password").expect("open");
        assert_eq!(
            again.get("proxyapi").expect("key").expose_secret(),
            "sk-second"
        );
        assert!(!temp.exists(), "the temp file must be consumed by the save");
    }
}
