//! What signing in keeps between runs, under `~/.config/virtx/`: which provider was signed in
//! to (`oidc.json`), the session's refresh token -- in the OS's keychain, or where the OS has
//! none in `<provider>.token` -- and the ID token last issued (`<provider>.id`). Each file is
//! readable by its owner alone.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

const KEYCHAIN: &str = "virtx";

/// The provider signed in to, and the client signed in as.
#[derive(Clone, Deserialize, Serialize)]
pub struct SignIn {
    pub issuer: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

/// An ID token kept between runs of `virtx token`.
#[derive(Deserialize, Serialize)]
pub struct Kept {
    pub token: String,
    /// Seconds since the epoch.
    pub expires_at: u64,
}

pub struct Session {
    dir: PathBuf,
    /// The provider, as a file's name.
    key: String,
    keychain: bool,
}

/// `~/.config/virtx`.
pub fn dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("no HOME to keep the session under")?;
    Ok(PathBuf::from(home).join(".config/virtx"))
}

/// The provider last signed in to, as kept in `dir`.
pub fn signed_in(dir: &Path) -> Option<SignIn> {
    serde_json::from_slice(&std::fs::read(dir.join("oidc.json")).ok()?).ok()
}

pub fn keep_sign_in(dir: &Path, sign_in: &SignIn) -> anyhow::Result<()> {
    write_private(&dir.join("oidc.json"), &serde_json::to_vec(sign_in)?)
}

impl Session {
    /// The session with `issuer`, kept in `dir`: its refresh token in the keychain if
    /// `keychain`, and the OS has one.
    pub fn new(dir: &Path, issuer: &str, keychain: bool) -> Session {
        let key = issuer
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .chars()
            .map(|c| match c {
                c if c.is_ascii_alphanumeric() || c == '.' || c == '-' => c,
                _ => '_',
            })
            .collect();
        Session {
            dir: dir.to_owned(),
            key,
            keychain,
        }
    }

    fn file(&self, extension: &str) -> PathBuf {
        self.dir.join(format!("{}.{extension}", self.key))
    }

    /// Held until dropped: no other run of `virtx token` reads or replaces the session meanwhile.
    pub fn lock(&self) -> anyhow::Result<File> {
        let path = self.file("lock");
        std::fs::create_dir_all(&self.dir)?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(file)
    }

    pub fn kept_id(&self) -> Option<Kept> {
        serde_json::from_slice(&std::fs::read(self.file("id")).ok()?).ok()
    }

    pub fn keep_id(&self, kept: &Kept) -> anyhow::Result<()> {
        write_private(&self.file("id"), &serde_json::to_vec(kept)?)
    }

    pub fn forget_id(&self) -> anyhow::Result<()> {
        remove(&self.file("id"))
    }

    fn entry(&self) -> Option<keyring::Entry> {
        match self.keychain {
            true => keyring::Entry::new(KEYCHAIN, &self.key).ok(),
            false => None,
        }
    }

    pub fn keep(&self, refresh: &str) -> anyhow::Result<()> {
        if let Some(entry) = self.entry()
            && entry.set_password(refresh).is_ok()
        {
            return Ok(());
        }
        write_private(&self.file("token"), refresh.as_bytes())
    }

    pub fn kept(&self) -> anyhow::Result<Option<String>> {
        if let Some(entry) = self.entry()
            && let Ok(refresh) = entry.get_password()
        {
            return Ok(Some(refresh));
        }
        match std::fs::read_to_string(self.file("token")) {
            Ok(refresh) => Ok(Some(refresh.trim().to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn forget(&self) -> anyhow::Result<()> {
        if let Some(entry) = self.entry() {
            let _ = entry.delete_credential();
        }
        remove(&self.file("token"))
    }
}

fn remove(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Write `bytes` to `path`, readable by its owner alone.
fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().expect("a directory"))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(path)?, bytes)
        .with_context(|| format!("writing {}", path.display()))
}
