//! The stores a volume's mounts are made of: one of a fixed few, each read-only and each
//! addressed by path, `/` its root.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use virtx_core::MountSource;

use super::volume::{Error, Result};
use super::{Dirent, Gdrive, GdriveConfig, Notion, NotionConfig, Onedrive, OnedriveConfig, Stat};

/// Where Google's services are reached, as JSON [`GdriveOrigins`](super::GdriveOrigins): the
/// server's setting, never a request's, since a token endpoint is sent the client secret.
pub const GDRIVE_ORIGINS_ENV: &str = "VIRTX_UVM_GDRIVE_ORIGINS";

/// Where Microsoft's services are reached, as JSON [`OnedriveOrigins`](super::OnedriveOrigins).
pub const ONEDRIVE_ORIGINS_ENV: &str = "VIRTX_UVM_ONEDRIVE_ORIGINS";

/// A remote store, as a [`MountSource`] names one.
pub enum Remote {
    Notion(Notion),
    Gdrive(Gdrive),
    Onedrive(Onedrive),
}

impl Remote {
    /// The store `source` names, refused when it cannot be made.
    pub fn new(source: &MountSource) -> Result<Remote> {
        let refused = |e: anyhow::Error| Error::Source(format!("{e:#}"));
        Ok(match source {
            MountSource::Notion { api_key } => Remote::Notion(
                Notion::new(&NotionConfig {
                    api_key: api_key.clone(),
                })
                .map_err(|e| refused(e.into()))?,
            ),
            MountSource::Gdrive {
                client_id,
                client_secret,
                refresh_token,
            } => Remote::Gdrive(
                Gdrive::new(&GdriveConfig {
                    client_id: client_id.clone(),
                    client_secret: client_secret.clone(),
                    refresh_token: refresh_token.clone(),
                    origins: origins(GDRIVE_ORIGINS_ENV)?,
                })
                .map_err(refused)?,
            ),
            MountSource::Onedrive {
                client_id,
                client_secret,
                refresh_token,
            } => Remote::Onedrive(
                Onedrive::new(&OnedriveConfig {
                    client_id: client_id.clone(),
                    client_secret: client_secret.clone(),
                    refresh_token: refresh_token.clone(),
                    origins: origins(ONEDRIVE_ORIGINS_ENV)?,
                })
                .map_err(refused)?,
            ),
        })
    }

    /// The store `source` names, made the first time `key` asks for it and kept for the life
    /// of the process, so its access token and listings are reused across calls.
    pub fn shared(key: String, source: &MountSource) -> Result<Arc<Remote>> {
        static STORES: OnceLock<Mutex<HashMap<String, Arc<Remote>>>> = OnceLock::new();
        let mut stores = STORES.get_or_init(Default::default).lock().unwrap();
        if let Some(store) = stores.get(&key) {
            return Ok(store.clone());
        }
        let store = Arc::new(Remote::new(source)?);
        stores.insert(key, store.clone());
        Ok(store)
    }

    /// Metadata for one entry, a file or a directory.
    pub async fn stat(&self, path: &Path) -> io::Result<Stat> {
        match self {
            Remote::Notion(store) => store.stat(path).await,
            Remote::Gdrive(store) => store.stat(path).await,
            Remote::Onedrive(store) => store.stat(path).await,
        }
    }

    /// The entries directly under `path`, each with its metadata where the listing had it
    /// for free -- see [`Dirent::stat`].
    pub async fn list(&self, path: &Path) -> io::Result<Vec<Dirent>> {
        match self {
            Remote::Notion(store) => store.list(path).await,
            Remote::Gdrive(store) => store.list(path).await,
            Remote::Onedrive(store) => store.list(path).await,
        }
    }

    /// Read into `buf` at `offset`, answering the bytes read.
    ///
    /// **A short return means EOF and nothing else.** Consumers pass the length straight on,
    /// so a store serving part of a request from cache fills the rest first, or the file
    /// appears to end at a cache boundary.
    pub async fn read_at(&self, path: &Path, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        match self {
            Remote::Notion(store) => store.read_at(path, buf, offset).await,
            Remote::Gdrive(store) => store.read_at(path, buf, offset).await,
            Remote::Onedrive(store) => store.read_at(path, buf, offset).await,
        }
    }

    /// Drop whatever is being kept, so the next read asks the source again. Best effort.
    pub async fn forget(&self) {
        if let Remote::Notion(store) = self {
            store.forget().await;
        }
    }
}

/// The origins `env` names, or the services' own without it.
fn origins<T: Default + serde::de::DeserializeOwned>(env: &'static str) -> Result<T> {
    match std::env::var(env) {
        Ok(json) => serde_json::from_str(&json).map_err(|e| Error::Origins(env, e)),
        Err(_) => Ok(T::default()),
    }
}
