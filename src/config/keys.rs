//! API key resolution: environment -> OS keyring -> file.
//!
//! Keys are wrapped in `secrecy` and wiped with `zeroize`; they must never reach model
//! context, logs, the TUI, or saved sessions (`AGENTS.md` §2). Only the resolved handle
//! may leave this module, never the plaintext.

use std::path::PathBuf;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};

/// Where a vendor's API key comes from.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    /// Environment variable, e.g. `PROXYAPI_KEY`.
    Env(String),
    /// OS keyring entry.
    Keyring { service: String, user: String },
    /// Plain file containing only the key (trimmed).
    File(PathBuf),
}

impl KeySource {
    /// Loads the key. `Ok(None)` means "configured but absent", which is not an error:
    /// offline commands such as `--test` keep working without a key.
    pub fn resolve(&self) -> Result<Option<SecretString>, KeyError> {
        match self {
            Self::Env(var) => Ok(std::env::var(var)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(SecretString::from)),

            Self::Keyring { service, user } => {
                let entry =
                    keyring::Entry::new(service, user).map_err(|source| KeyError::Keyring {
                        service: service.clone(),
                        user: user.clone(),
                        source,
                    })?;

                match entry.get_password() {
                    Ok(password) if !password.trim().is_empty() => {
                        Ok(Some(SecretString::from(password)))
                    }
                    Ok(_) => Ok(None),
                    Err(keyring::Error::NoEntry) => Ok(None),
                    Err(source) => Err(KeyError::Keyring {
                        service: service.clone(),
                        user: user.clone(),
                        source,
                    }),
                }
            }

            Self::File(path) => {
                if !path.is_file() {
                    return Ok(None);
                }
                let text = std::fs::read_to_string(path).map_err(|source| KeyError::File {
                    path: path.clone(),
                    source,
                })?;
                let key = text.trim();
                if key.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(SecretString::from(key.to_owned())))
                }
            }
        }
    }

    /// Description safe to print: names the source, never the secret.
    pub fn describe(&self) -> String {
        match self {
            Self::Env(var) => format!("env {var}"),
            Self::Keyring { service, user } => format!("keyring {service}/{user}"),
            Self::File(path) => format!("file {}", path.display()),
        }
    }
}

/// Key resolution errors. The message never includes the secret itself.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("failed to read key file {path}: {source}")]
    File {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("keyring lookup failed for {service}/{user}: {source}")]
    Keyring {
        service: String,
        user: String,
        #[source]
        source: keyring::Error,
    },
}
