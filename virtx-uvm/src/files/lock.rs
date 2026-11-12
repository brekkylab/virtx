//! Conditional writes to one path, one at a time: a request that writes with `If-Match` or
//! `If-None-Match` while another such request to its path is under way is refused, rather
//! than checked against a file the other is about to replace. dav-server checks the condition
//! before it opens the file and moves the write into place only once the whole body has
//! arrived, so two requests that checked one state would both write, the later over the
//! earlier. Refused, not waited for: the second would almost always find its condition broken by
//! the first, and a slow or stalled body would hold every other writer of the file.
//!
//! A volume's are held by a lock file per path, which another process over the home sees, and
//! the system lets go of with a process that ends. A sandbox's are held in this process, the
//! one its machine is in.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::Mutex;

use dav_server::davpath::DavPath;
use http::{Method, Request, header};
use unicode_normalization::UnicodeNormalization as _;

/// The key of `request`'s path in the root at `prefix` if the request is a conditional write,
/// one that changes what is at the path and carries `If-Match` or `If-None-Match`. A read with
/// either is not one: `If-None-Match` on a `GET` is a cache asking whether it is current.
///
/// The path is read as dav-server reads it, decoded with `.` and `..` resolved, and then
/// composed and lowercased, so the spellings of one file on a host that folds case or
/// normalizes names, as macOS does, have one key. Two files whose names differ only so share
/// it.
pub(super) fn conditional<B>(prefix: &str, request: &Request<B>) -> Option<String> {
    let method = request.method();
    let reads = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
        || method.as_str() == "PROPFIND";
    let headers = request.headers();
    let condition =
        headers.contains_key(header::IF_MATCH) || headers.contains_key(header::IF_NONE_MATCH);
    if reads || !condition {
        return None;
    }
    // One dav-server cannot read, or not under the prefix, it refuses itself.
    let path = DavPath::new(request.uri().path()).ok()?;
    let rest = path
        .with_prefix()
        .as_bytes()
        .strip_prefix(prefix.as_bytes())?;
    let rest = String::from_utf8_lossy(rest);
    Some(
        rest.trim_end_matches('/')
            .nfc()
            .flat_map(char::to_lowercase)
            .collect(),
    )
}

/// Hold `key`'s lock among those of the volume whose own directory is `dir`, let go when the
/// file drops; `None` while another request, in this process or another, holds it.
///
/// Each key has a file of its own, so a write is refused only for a write to its own path. The
/// file is left after: removed, one process could hold it as it was opened before the removal
/// and another as it is made again. The volume's removal takes them with it.
pub(super) fn volume(dir: &Path, key: &str) -> io::Result<Option<File>> {
    let locks = dir.join("locks");
    fs::create_dir_all(&locks)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(locks.join(name(key)))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Ok(None),
        Err(fs::TryLockError::Error(e)) => Err(e),
    }
}

/// The name of `key`'s lock file: its 64-bit FNV-1a, which is the same in every build, so
/// servers of two versions over one home pick one file for one path, and two paths fall on one
/// file too seldom to be seen.
fn name(key: &str) -> String {
    let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{hash:016x}")
}

/// The keys of the writes under way, each held by one request at a time.
#[derive(Default)]
pub(super) struct Keyed(Mutex<HashSet<String>>);

/// [`Keyed`]'s hold of one key, let go when this drops.
pub(super) struct Hold<'a> {
    keyed: &'a Keyed,
    key: String,
}

impl Keyed {
    /// Hold `key`; `None` while another holds it.
    pub(super) fn hold(&self, key: String) -> Option<Hold<'_>> {
        let held = self.0.lock().unwrap().insert(key.clone());
        held.then_some(Hold { keyed: self, key })
    }
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        self.keyed.0.lock().unwrap().remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, path: &str, condition: Option<header::HeaderName>) -> Request<()> {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(name) = condition {
            request = request.header(name, "*");
        }
        request.body(()).unwrap()
    }

    #[test]
    fn a_conditional_write_is_keyed_by_its_file_however_it_is_spelled() {
        let key = |path| {
            conditional(
                "/volumes/v/files",
                &request("PUT", path, Some(header::IF_NONE_MATCH)),
            )
        };
        for spelled in [
            "/volumes/v/files/dir/A.txt",
            "/volumes/v/files/dir/a.txt",
            "/volumes/v/files//dir/./a.txt/",
            "/volumes/v/files/dir/%41.txt",
            "/volumes/v/files/x/../dir/a.txt",
        ] {
            assert_eq!(key(spelled).as_deref(), Some("/dir/a.txt"), "{spelled}");
        }
        // Composed and decomposed, as macOS answers a name.
        assert_eq!(
            key("/volumes/v/files/caf%C3%A9"),
            key("/volumes/v/files/cafe%CC%81")
        );
        assert_eq!(key("/volumes/v/files").as_deref(), Some(""));
        // Not under the prefix: dav-server refuses it, and nothing is held.
        assert_eq!(key("/volumes/w/files/a"), None);
    }

    #[test]
    fn only_a_write_with_a_condition_is_held() {
        let at = "/volumes/v/files/a";
        let key =
            |method, condition| conditional("/volumes/v/files", &request(method, at, condition));
        for method in ["PUT", "DELETE", "MOVE", "COPY", "PROPPATCH", "MKCOL"] {
            assert!(key(method, Some(header::IF_MATCH)).is_some(), "{method}");
            assert!(
                key(method, Some(header::IF_NONE_MATCH)).is_some(),
                "{method}"
            );
            assert_eq!(key(method, None), None, "{method}");
        }
        for method in ["GET", "HEAD", "OPTIONS", "PROPFIND"] {
            assert_eq!(key(method, Some(header::IF_NONE_MATCH)), None, "{method}");
            assert_eq!(key(method, Some(header::IF_MATCH)), None, "{method}");
        }
    }

    #[test]
    fn a_key_names_one_lock_file_in_every_build() {
        // FNV-1a's offset basis: a change of hash would rename every file.
        assert_eq!(name(""), "cbf29ce484222325");
        assert_eq!(name("/a"), name("/a"));
        assert_ne!(name("/a"), name("/b"));
    }

    #[test]
    fn a_volume_s_lock_is_refused_while_another_holds_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let held = volume(dir.path(), "/a").unwrap().expect("nobody holds it");
        // Another open of the file, as another process's is, is refused while this holds it.
        assert!(volume(dir.path(), "/a").unwrap().is_none());
        let other = File::open(dir.path().join("locks").join(name("/a"))).unwrap();
        assert!(matches!(
            other.try_lock(),
            Err(fs::TryLockError::WouldBlock)
        ));
        // Another path is not.
        assert!(volume(dir.path(), "/b").unwrap().is_some());
        drop(held);
        assert!(volume(dir.path(), "/a").unwrap().is_some());
    }

    #[test]
    fn a_key_is_refused_while_held_and_free_once_let_go() {
        let keyed = Keyed::default();
        let held = keyed.hold("sb\0/a".into()).expect("nobody holds it");
        assert!(keyed.hold("sb\0/a".into()).is_none());
        assert!(keyed.hold("sb\0/b".into()).is_some());
        drop(held);
        assert!(keyed.hold("sb\0/a".into()).is_some());
        assert!(keyed.0.lock().unwrap().is_empty());
    }
}
