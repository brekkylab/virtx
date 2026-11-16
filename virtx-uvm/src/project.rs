//! Projects: what everything a server keeps belongs to. Each project is a directory of its own,
//! which its images, snapshots, sandboxes and volumes are kept in, and each has members, each
//! in a role, who alone reach it.
//!
//! * [`Store`]: the projects' directories under a server's root, made, listed and removed by
//!   name, as snapshots and volumes are.
//! * [`Membership`]: who is a member of which project, in which role. A server keeps it as it
//!   chooses -- in a file beside each project's directory, or in a database -- and asks it of
//!   every request.
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   project/<name>/     a project's own: images/, snapshot/, sandbox/, volume/
//! ```

use std::fs;
use std::io;
use std::path::PathBuf;

use futures_util::future::BoxFuture;
use virtx_core::Role;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "{0:?} is not a project's name: lower-case letters, digits, '.', '_' and '-', not \
         starting with '.' or '-'"
    )]
    InvalidName(String),

    #[error("no project here is named {0}")]
    Unknown(String),

    #[error("a project named {0} is already here")]
    AlreadyExists(String),

    #[error("project {0} would be left with no owner")]
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

/// The projects' directories under a server's root.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Store {
        Store { root: root.into() }
    }

    /// Whether `name` can name a project: a [name](crate::is_name), in lower case, since it is a
    /// directory's and two that differ in case alone are one on a filesystem that ignores case.
    pub fn is_name(name: &str) -> bool {
        crate::is_name(name) && !name.bytes().any(|b| b.is_ascii_uppercase())
    }

    /// Project `name`'s directory, there or not: refused unless `name` [is one](Store::is_name).
    pub fn path(&self, name: &str) -> Result<PathBuf> {
        match Store::is_name(name) {
            true => Ok(self.root.join("project").join(name)),
            false => Err(Error::InvalidName(name.to_owned())),
        }
    }

    /// Whether project `name` has its directory here.
    pub fn exists(&self, name: &str) -> bool {
        self.path(name).is_ok_and(|dir| dir.is_dir())
    }

    /// Every project here, by name, in order.
    pub fn list(&self) -> Result<Vec<String>> {
        let dir = self.root.join("project");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::io(format!("reading {}", dir.display()), e)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| Error::io(format!("reading {}", dir.display()), e))?;
            if let Some(name) = entry.file_name().to_str()
                && Store::is_name(name)
                && entry.path().is_dir()
            {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    /// Make project `name`'s directory, empty, and answer it.
    pub fn create(&self, name: &str) -> Result<PathBuf> {
        let dir = self.path(name)?;
        let parent = self.root.join("project");
        fs::create_dir_all(&parent)
            .map_err(|e| Error::io(format!("making {}", parent.display()), e))?;
        // Made exclusively: two creates of one name cannot both win.
        match fs::create_dir(&dir) {
            Ok(()) => Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                Err(Error::AlreadyExists(name.to_owned()))
            }
            Err(e) => Err(Error::io(format!("making {}", dir.display()), e)),
        }
    }

    /// Delete project `name`'s directory and everything in it. Whoever removes it closes what
    /// runs over it first: a sandbox's machine still running there keeps the files it has open.
    pub fn remove(&self, name: &str) -> Result<()> {
        let dir = self.path(name)?;
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Error::Unknown(name.to_owned())),
            Err(e) => Err(Error::io(format!("removing {}", dir.display()), e)),
        }
    }
}

/// Who is a member of which project, in which role.
pub trait Membership: Send + Sync {
    /// The role `user` has in `project`: `None` where they have none, or it does not exist,
    /// the one not told apart from the other.
    fn role<'a>(&'a self, user: &'a str, project: &'a str) -> BoxFuture<'a, Result<Option<Role>>>;

    /// Every project `user` has a role in, with the role, by name.
    fn projects<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<Vec<(String, Role)>>>;

    /// Begin `project`'s members, with `owner` the one, its owner: refused where it has members.
    fn create<'a>(&'a self, project: &'a str, owner: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Forget `project`'s members, so that no one reaches it.
    fn remove<'a>(&'a self, project: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Every member of `project`, with their role, by user.
    fn members<'a>(&'a self, project: &'a str) -> BoxFuture<'a, Result<Vec<(String, Role)>>>;

    /// Give `user` `role` in `project`, or with `None` take theirs away: refused where it would
    /// leave the project no owner.
    fn set_member<'a>(
        &'a self,
        project: &'a str,
        user: &'a str,
        role: Option<Role>,
    ) -> BoxFuture<'a, Result<()>>;
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn a_project_s_name_is_lower_case_and_its_directory_is_under_project() {
        let store = Store::new("/home");
        assert_eq!(
            store.path("demo-1.x_y").unwrap(),
            Path::new("/home/project/demo-1.x_y")
        );
        for name in ["Demo", "", ".hidden", "-x", "a/b", ".."] {
            assert!(
                matches!(store.path(name), Err(Error::InvalidName(_))),
                "{name:?}"
            );
        }
    }

    #[test]
    fn makes_lists_and_removes_projects() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        assert_eq!(store.list().unwrap(), [] as [&str; 0]);

        let made = store.create("demo").unwrap();
        assert!(made.is_dir() && store.exists("demo"));
        assert!(matches!(store.create("demo"), Err(Error::AlreadyExists(_))));
        store.create("other").unwrap();
        // What can be no project's is not one.
        fs::create_dir(root.path().join("project/Upper")).unwrap();
        fs::write(root.path().join("project/file"), "").unwrap();
        assert_eq!(store.list().unwrap(), ["demo", "other"]);

        fs::write(made.join("kept"), "").unwrap();
        store.remove("demo").unwrap();
        assert!(!made.exists() && !store.exists("demo"));
        assert!(matches!(store.remove("demo"), Err(Error::Unknown(_))));
    }
}
