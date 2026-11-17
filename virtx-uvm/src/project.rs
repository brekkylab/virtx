//! Projects: what everything a server keeps belongs to. Each project is a directory of its own,
//! under an id given it once, which its images, snapshots, sandboxes and volumes are kept in.
//! Each is named by its owner, and has members, each in a role, who alone reach it.
//!
//! * [`Store`]: the projects' directories under a server's root, by id, as snapshots and
//!   volumes are kept by name.
//! * [`Membership`]: which project an owner names so, and who is a member of each, in which
//!   role. A server keeps it as it chooses -- in files in each project's directory, or in a
//!   database -- and asks it of every request.
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   project/<id>/       a project's own: images/, snapshot/, sandbox/, volume/, and what a
//!                       membership keeps of it there
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::str::FromStr;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use virtx_core::Role;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "{0:?} is not a project's name: lower-case letters, digits, '.', '_' and '-', not \
         starting with '.' or '-', and no project's id"
    )]
    InvalidName(String),

    #[error("no project here is {0}")]
    Unknown(String),

    #[error("a project named {0} is already its owner's")]
    AlreadyExists(String),

    #[error("project {0} would be left without its owner")]
    NoOwner(String),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },

    /// A membership's own failure, such as its database's.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    pub fn io(context: impl Into<String>, source: io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Whether `name` can name a project: a [name](crate::is_name), in lower case, and none a
/// [`ProjectId`] is read from, so that what names a project is one or the other.
pub fn is_name(name: &str) -> bool {
    crate::is_name(name)
        && !name.bytes().any(|b| b.is_ascii_uppercase())
        && Uuid::parse_str(name).is_err()
}

/// A project's id: given it once, as it is made, and never another's, spelled as a UUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectId(Uuid);

impl ProjectId {
    /// An id no project has had.
    pub fn new() -> ProjectId {
        ProjectId(Uuid::new_v4())
    }
}

impl Default for ProjectId {
    fn default() -> ProjectId {
        ProjectId::new()
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.hyphenated().fmt(f)
    }
}

impl FromStr for ProjectId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> std::result::Result<ProjectId, uuid::Error> {
        Uuid::parse_str(s).map(ProjectId)
    }
}

/// Whose a project is, whose names it is among: no two of one owner's share a name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Owner {
    /// A user, as the server names them, who is an owner of each project of theirs.
    User(String),
}

/// A project, as a [`Membership`] keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub id: ProjectId,
    pub owner: Owner,
    pub name: String,
    /// Each member's role, by user: they alone reach it.
    pub members: BTreeMap<String, Role>,
}

/// The projects' directories under a server's root.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Store {
        Store { root: root.into() }
    }

    /// Where the projects' directories are: `project/` under the server's root.
    pub fn dir(&self) -> PathBuf {
        self.root.join("project")
    }

    /// Project `id`'s directory, there or not: made as what is kept in it is.
    pub fn path(&self, id: &ProjectId) -> PathBuf {
        self.dir().join(id.to_string())
    }

    /// Every project with a directory here, in order.
    pub fn list(&self) -> Result<Vec<ProjectId>> {
        let dir = self.dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::io(format!("reading {}", dir.display()), e)),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(format!("reading {}", dir.display()), e))?;
            if let Some(name) = entry.file_name().to_str()
                && let Ok(id) = name.parse::<ProjectId>()
                && id.to_string() == name
                && entry.path().is_dir()
            {
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Delete project `id`'s directory and everything in it, if it has one. Whoever removes it
    /// closes what runs over it first: a sandbox's machine still running there keeps the files
    /// it has open.
    pub fn remove(&self, id: &ProjectId) -> Result<()> {
        let dir = self.path(id);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io(format!("removing {}", dir.display()), e)),
        }
    }
}

/// Which project an owner names so, and who is a member of each, in which role.
///
/// A project is forgotten ([`remove`](Membership::remove)) before its directory is deleted, so
/// that no request reaches what is being deleted; a directory left by a removal that did not
/// end is no project's, and reached by no request, since no id is given twice.
pub trait Membership: Send + Sync {
    /// The project `owner` names `name`, if any. A user's [`virtx_core::DEFAULT_PROJECT`] is
    /// always there: made, the user its owner, as it is first resolved.
    fn resolve<'a>(
        &'a self,
        owner: &'a Owner,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProjectId>>>;

    /// Project `id`, with its members, if there is one.
    fn project<'a>(&'a self, id: &'a ProjectId) -> BoxFuture<'a, Result<Option<Project>>>;

    /// Make project `name` of `owner`'s, under an id no project has had, with `creator` its one
    /// member, an owner: refused where `owner` has a project of that name.
    fn create<'a>(
        &'a self,
        owner: &'a Owner,
        name: &'a str,
        creator: &'a str,
    ) -> BoxFuture<'a, Result<Project>>;

    /// Every project `user` is a member of, by name.
    fn projects<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<Vec<Project>>>;

    /// Give `user` `role` in project `id`, or with `None` take theirs away: refused where it
    /// would take the project from its owner.
    fn set_member<'a>(
        &'a self,
        id: &'a ProjectId,
        user: &'a str,
        role: Option<Role>,
    ) -> BoxFuture<'a, Result<()>>;

    /// Forget project `id`, its name its owner's to give again, so that no one reaches it.
    fn remove<'a>(&'a self, id: &'a ProjectId) -> BoxFuture<'a, Result<()>>;
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn a_project_s_name_is_lower_case_and_no_id_and_its_directory_is_its_id() {
        let id: ProjectId = "0f6a3b8e-1c2d-4e5f-8a9b-0c1d2e3f4a5b".parse().unwrap();
        assert_eq!(
            Store::new("/home").path(&id),
            Path::new("/home/project/0f6a3b8e-1c2d-4e5f-8a9b-0c1d2e3f4a5b")
        );
        assert!(is_name("demo-1.x_y"));
        for name in [
            "Demo",
            "",
            ".hidden",
            "-x",
            "a/b",
            "..",
            "0f6a3b8e-1c2d-4e5f-8a9b-0c1d2e3f4a5b",
            "0f6a3b8e1c2d4e5f8a9b0c1d2e3f4a5b",
        ] {
            assert!(!is_name(name), "{name:?}");
        }
    }

    #[test]
    fn lists_and_removes_projects_directories_by_id() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        assert_eq!(store.list().unwrap(), []);

        let (demo, other) = (ProjectId::new(), ProjectId::new());
        for id in [demo, other] {
            fs::create_dir_all(store.path(&id)).unwrap();
        }
        // What can be no project's is not one.
        fs::create_dir(root.path().join("project/demo")).unwrap();
        fs::write(root.path().join("project/file"), "").unwrap();
        let mut both = vec![demo, other];
        both.sort();
        assert_eq!(store.list().unwrap(), both);

        fs::write(store.path(&demo).join("kept"), "").unwrap();
        store.remove(&demo).unwrap();
        assert!(!store.path(&demo).exists());
        store.remove(&demo).unwrap();
        assert_eq!(store.list().unwrap(), [other]);
    }
}
