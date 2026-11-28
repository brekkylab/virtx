//! Grants: each sandbox made of a package, given to who made it, as they were let in. A request
//! to the sandbox from one who is no member of its project is let in by its grant: sent with a
//! token the same way in lets in, of the same issuer and subject. A home keeps one grant per
//! sandbox, and takes it away with the sandbox.
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   grant/<sandbox>.json   the grant of the sandbox <sandbox>
//! ```

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use virtx_core::{PackageAccess, SandboxResizeArgs};

const GRANT: &str = "grant";

/// Who makes a sandbox of a package, as [`Access::maker`](crate::access::Access::maker) found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Maker {
    /// A member of its project, by their user.
    Member { user: String },
    /// One its access let in: the way in that let them in, and their token's `sub`.
    LetIn { way: PackageAccess, subject: String },
}

impl Maker {
    /// Who one let in is, whichever of the package's ways let them in: their token's issuer and
    /// subject. `None` for a member, who counts toward none of the package's limits.
    pub fn subject(&self) -> Option<(&str, &str)> {
        match self {
            Maker::Member { .. } => None,
            Maker::LetIn {
                way: PackageAccess::Jwt { issuer, .. },
                subject,
            } => Some((issuer, subject)),
        }
    }
}

/// A sandbox made of a package, given to who made it, and what they may do to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// The sandbox.
    pub sandbox: String,
    /// The package it was made of, by id.
    pub package: String,
    /// Who made it.
    pub maker: Maker,
    /// How large one let in may resize it to, each member given.
    pub max: SandboxResizeArgs,
    /// The origins a web page of theirs may reach it from, beside the server's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origins: Vec<String>,
}

/// The sandbox a request to `path` with `method` is to, where one let in may send it: its
/// `GET`, `PATCH` and `DELETE`, and everything under it. Not the listing of the sandboxes, nor
/// an id that is no sandbox's.
pub fn sandbox_of<'a>(method: &http::Method, path: &'a str) -> Option<&'a str> {
    use http::Method;
    let rest = path.strip_prefix("/sandboxes/")?;
    let id = match rest.split_once('/') {
        None if [Method::GET, Method::PATCH, Method::DELETE].contains(method) => rest,
        None => return None,
        Some((id, _)) => id,
    };
    is_id(id).then_some(id)
}

/// Whether `id` could be a sandbox's: letters, digits, `_` and `-`, which a file is named by.
fn is_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// The grants of one project, kept under its root.
#[derive(Clone, Debug)]
pub struct Grants {
    dir: PathBuf,
}

impl Grants {
    /// The grants kept under `root`.
    pub fn new(root: &Path) -> io::Result<Grants> {
        let dir = root.join(GRANT);
        fs::create_dir_all(&dir)?;
        Ok(Grants { dir })
    }

    /// Keep `grant`, in place of what its sandbox had.
    pub fn keep(&self, grant: &Grant) -> io::Result<()> {
        let path = self
            .path(&grant.sandbox)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no sandbox's id"))?;
        let json = serde_json::to_vec_pretty(grant).map_err(io::Error::other)?;
        let part = path.with_extension("json.part");
        fs::write(&part, json)?;
        fs::rename(&part, &path)
    }

    /// The grant of sandbox `sandbox`, if it was made of a package.
    pub fn find(&self, sandbox: &str) -> io::Result<Option<Grant>> {
        let Some(path) = self.path(sandbox) else {
            return Ok(None);
        };
        match fs::read(path) {
            Ok(json) => Ok(Some(
                serde_json::from_slice(&json).map_err(io::Error::other)?,
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Every grant here, by its sandbox's id.
    pub fn list(&self) -> io::Result<Vec<Grant>> {
        let mut grants: Vec<Grant> = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                grants.push(serde_json::from_slice(&fs::read(&path)?).map_err(io::Error::other)?);
            }
        }
        grants.sort_by(|a, b| a.sandbox.cmp(&b.sandbox));
        Ok(grants)
    }

    /// Take away the grant of sandbox `sandbox`, if it has one.
    pub fn take_away(&self, sandbox: &str) -> io::Result<()> {
        let Some(path) = self.path(sandbox) else {
            return Ok(());
        };
        match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn path(&self, sandbox: &str) -> Option<PathBuf> {
        is_id(sandbox).then(|| self.dir.join(format!("{sandbox}.json")))
    }
}

#[cfg(test)]
mod tests {
    use http::Method;

    use super::*;

    fn grant(sandbox: &str) -> Grant {
        Grant {
            sandbox: sandbox.into(),
            package: "p".into(),
            maker: Maker::Member {
                user: "alice".into(),
            },
            max: SandboxResizeArgs::default().vcpus(2),
            origins: Vec::new(),
        }
    }

    /// A grant is found by its sandbox's id, and taken away with it; an id that is no
    /// sandbox's names no file.
    #[test]
    fn a_grant_is_found_by_its_sandbox_and_taken_away_with_it() {
        let root = tempfile::tempdir().unwrap();
        let store = Grants::new(root.path()).unwrap();
        store.keep(&grant("b")).unwrap();
        store.keep(&grant("a")).unwrap();
        assert_eq!(store.find("a").unwrap(), Some(grant("a")));
        assert_eq!(store.find("../grant/a").unwrap(), None);
        assert!(store.keep(&grant("../a")).is_err());
        assert_eq!(store.list().unwrap(), [grant("a"), grant("b")]);

        store.take_away("a").unwrap();
        store.take_away("a").unwrap();
        assert_eq!(store.find("a").unwrap(), None);
        assert_eq!(store.list().unwrap(), [grant("b")]);
    }

    /// One let in reaches a sandbox's own requests, and not the listing of them all.
    #[test]
    fn one_let_in_sends_a_sandbox_s_own_requests_alone() {
        assert_eq!(sandbox_of(&Method::GET, "/sandboxes/a"), Some("a"));
        assert_eq!(sandbox_of(&Method::DELETE, "/sandboxes/a"), Some("a"));
        assert_eq!(
            sandbox_of(&Method::POST, "/sandboxes/a/jobs/exec/"),
            Some("a")
        );
        assert_eq!(sandbox_of(&Method::POST, "/sandboxes/a"), None);
        assert_eq!(sandbox_of(&Method::GET, "/sandboxes/"), None);
        assert_eq!(sandbox_of(&Method::GET, "/sandboxes/../volumes/"), None);
        assert_eq!(sandbox_of(&Method::GET, "/volumes/"), None);
    }
}
