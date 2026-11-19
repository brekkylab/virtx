//! What signing in keeps between runs, under `~/.config/virtx/`: what was signed in to last
//! (`signed-in.json`), and for each client of a provider signed in as, the session's refresh
//! token -- in the OS's keychain, or where the OS has none in `<client_id>@<issuer>.token` --
//! and the ID token last issued (`<client_id>@<issuer>.id`). The directory is its owner's
//! alone, each of those files is written readable by its owner alone, and one the directory's
//! owner does not own is not read: another user who could write the directory could put a
//! file of their own in place of one kept there, or leave one there before the directory was
//! made its owner's alone, and `virtx token` would then act with that user's session.

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
    /// The client and its provider, as a file's name: `<client_id>@<issuer>`.
    key: String,
    keychain: bool,
}

/// `~/.config/virtx`, made its owner's alone before anything in it is read.
pub fn dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("no HOME to keep the session under")?;
    let dir = PathBuf::from(home).join(".config/virtx");
    make_private(&dir)?;
    Ok(dir)
}

/// What was signed in to last: virtx cloud, as `virtx_core::cli` keeps it (`{"domain": ...}`),
/// or a provider.
#[derive(Deserialize)]
#[serde(untagged)]
pub enum SignedIn {
    Cloud {
        // What tells it from a provider; virtx cloud's commands take their domain themselves.
        #[allow(dead_code)]
        domain: String,
    },
    Provider(SignIn),
}

/// What was signed in to last, as kept in `dir`: none from a file its owner does not own.
pub fn signed_in(dir: &Path) -> Option<SignedIn> {
    serde_json::from_slice(&read_ours(&dir.join("signed-in.json")).ok()??).ok()
}

/// Keep `sign_in` as what was signed in to last.
pub fn keep_sign_in(dir: &Path, sign_in: &SignIn) -> anyhow::Result<()> {
    write_private(&dir.join("signed-in.json"), &serde_json::to_vec(sign_in)?)
}

impl Session {
    /// The session with `sign_in`'s provider as its client, kept in `dir`: its refresh token
    /// in the keychain if `keychain`, and the OS has one.
    pub fn new(dir: &Path, sign_in: &SignIn, keychain: bool) -> Session {
        let named = |text: &str| -> String {
            text.chars()
                .map(|c| match c {
                    c if c.is_ascii_alphanumeric() || c == '.' || c == '-' => c,
                    _ => '_',
                })
                .collect()
        };
        let issuer = sign_in.issuer.trim_start_matches("https://");
        let issuer = issuer.trim_start_matches("http://");
        // Two clients of one provider are two sessions; `@` is in neither part.
        let key = format!("{}@{}", named(&sign_in.client_id), named(issuer));
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
        make_private(&self.dir)?;
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
        serde_json::from_slice(&read_ours(&self.file("id")).ok()??).ok()
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
        match read_ours(&self.file("token"))? {
            Some(refresh) => Ok(Some(String::from_utf8(refresh)?.trim().to_owned())),
            None => Ok(None),
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

/// What the file at `path` holds, or `None` where there is none or its owner is not its
/// directory's: a file another user left there while the directory was open to them is not
/// opened. On Windows, read any file there.
fn read_ours(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        match std::fs::symlink_metadata(path) {
            Ok(file) => {
                let dir = path.parent().expect("a directory");
                // The link's owner, not its target's: a link another user left there is theirs.
                if file.uid() != std::fs::metadata(dir)?.uid() {
                    return Ok(None);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Make `dir`, with any parent it lacks, `0700`, and set it `0700` where it was left with any
/// mode for the group or others. On Windows, make it and read or set no mode.
fn make_private(dir: &Path) -> anyhow::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .with_context(|| format!("making {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("setting {} ({mode:o}) to 700", dir.display()))?;
        }
    }
    Ok(())
}

/// Write `bytes` to `path`, readable by its owner alone however a file there was left: written
/// to a new `0600` file beside it and put in its place.
fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().expect("a directory");
    make_private(dir)?;
    let name = path.file_name().expect("a file").to_string_lossy();
    let fresh = dir.join(format!(".{name}.{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let _ = std::fs::remove_file(&fresh);
    let written = options
        .open(&fresh)
        .and_then(|mut file| std::io::Write::write_all(&mut file, bytes))
        .and_then(|()| std::fs::rename(&fresh, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&fresh);
    }
    written.with_context(|| format!("writing {}", path.display()))
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// A path of its own under the system's temporary directory, with nothing there.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("virtx-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn sign_in() -> SignIn {
        SignIn {
            issuer: "https://idp.example".into(),
            client_id: "virtx".into(),
            client_secret: None,
        }
    }

    #[test]
    fn the_directory_a_session_is_kept_in_is_its_owner_s_alone_however_it_was_left() {
        let dir = scratch("dir-mode");
        let session = Session::new(&dir, &sign_in(), false);
        drop(session.lock().unwrap());
        assert_eq!(mode(&dir), 0o700);

        set_mode(&dir, 0o777);
        drop(session.lock().unwrap());
        assert_eq!(mode(&dir), 0o700);

        set_mode(&dir, 0o755);
        keep_sign_in(&dir, &sign_in()).unwrap();
        assert_eq!(mode(&dir), 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_kept_is_its_owner_s_alone_however_it_was_left() {
        let dir = scratch("file-mode");
        let session = Session::new(&dir, &sign_in(), false);
        let id = dir.join("virtx@idp.example.id");
        let token = dir.join("virtx@idp.example.token");
        let old = Kept {
            token: "id-old".into(),
            expires_at: 1,
        };
        session.keep_id(&old).unwrap();
        session.keep("r-old").unwrap();
        set_mode(&id, 0o644);
        set_mode(&token, 0o644);

        let new = Kept {
            token: "id-new".into(),
            expires_at: 2,
        };
        session.keep_id(&new).unwrap();
        session.keep("r-new").unwrap();
        assert_eq!(mode(&id), 0o600);
        assert_eq!(mode(&token), 0o600);
        assert_eq!(session.kept_id().unwrap().token, "id-new");
        assert_eq!(session.kept().unwrap().as_deref(), Some("r-new"));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "nothing left beside them"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_its_directory_s_owner_does_not_own_is_not_read() {
        use std::os::unix::fs::MetadataExt as _;
        let owner = |path: &Path| std::fs::symlink_metadata(path).unwrap().uid();
        // `/tmp` is root's, and the files made in it here are not, as files another user left
        // in a directory once open to them are not the directory's owner's.
        let tmp = Path::new("/tmp");
        let sign_in = SignIn {
            client_id: format!("virtx-cli-test-{}", std::process::id()),
            ..sign_in()
        };
        let session = Session::new(tmp, &sign_in, false);
        let id = session.file("id");
        let token = session.file("token");
        let left = Kept {
            token: "id-left".into(),
            expires_at: u64::MAX,
        };
        std::fs::write(&id, serde_json::to_vec(&left).unwrap()).unwrap();
        // A link of their own to a file root owns.
        std::os::unix::fs::symlink("/etc/hosts", &token).unwrap();
        for file in [&id, &token] {
            // Run as root, the test gives them to another user.
            if owner(file) == owner(tmp) {
                std::os::unix::fs::lchown(file, Some(1), None).unwrap();
            }
        }

        let read = (
            session.kept_id().map(|kept| kept.token),
            session.kept().map_err(|e| e.to_string()),
        );
        let _ = std::fs::remove_file(&id);
        let _ = std::fs::remove_file(&token);
        assert_eq!(read, (None, Ok(None)));
    }
}
