//! Projects: each one's own root under a server's, which everything it keeps -- its images,
//! snapshots, sandboxes and volumes -- is kept in, and who may reach each.
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   project/<name>/     its own root: images/, snapshot/, sandbox/, volume/
//! ```
//!
//! Who may reach a project is a [`ProjectStore`]'s to say, each server's own: one where a
//! project's directory is all there is to it, one that keeps its members beside its root, or
//! one that keeps them in a database.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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

    #[error("this server has no members: everyone is an owner of every project")]
    NoMembers,

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },

    /// A store's own failure, such as its database's.
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

/// Whether `name` can name a project: a [name](crate::is_name), in lower case, since it is a
/// directory's and two that differ in case alone are one on a filesystem that ignores case.
pub fn is_project_name(name: &str) -> bool {
    crate::is_name(name) && !name.bytes().any(|b| b.is_ascii_uppercase())
}

/// Project `name`'s own root under `root`, refused unless `name` [is one](is_project_name).
pub fn project_root(root: &Path, name: &str) -> Result<PathBuf, Error> {
    match is_project_name(name) {
        true => Ok(root.join("project").join(name)),
        false => Err(Error::InvalidName(name.to_owned())),
    }
}

/// The projects a server has, and who is a member of each, in which role.
pub trait ProjectStore: Send + Sync {
    /// The role `user` has in `project`: `None` where they have none, or it does not exist,
    /// the one not told apart from the other.
    fn role<'a>(
        &'a self,
        user: &'a str,
        project: &'a str,
    ) -> BoxFuture<'a, Result<Option<Role>, Error>>;

    /// Every project `user` has a role in, with the role, by name.
    fn projects<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<Vec<(String, Role)>, Error>>;

    /// Make `project`, with `owner` its one member, its owner.
    fn create<'a>(&'a self, project: &'a str, owner: &'a str) -> BoxFuture<'a, Result<(), Error>>;

    /// Forget `project`, so that no one reaches it. What is under its root is the caller's to
    /// remove after.
    fn remove<'a>(&'a self, project: &'a str) -> BoxFuture<'a, Result<(), Error>>;

    /// Every member of `project`, with their role, by user.
    fn members<'a>(&'a self, project: &'a str)
    -> BoxFuture<'a, Result<Vec<(String, Role)>, Error>>;

    /// Give `user` `role` in `project`, or with `None` take theirs away: refused where it would
    /// leave the project no owner.
    fn set_member<'a>(
        &'a self,
        project: &'a str,
        user: &'a str,
        role: Option<Role>,
    ) -> BoxFuture<'a, Result<(), Error>>;
}

/// The name of each directory under `root`'s `project/` that can be a project's, in no set
/// order: none where there is no `project/`.
pub fn project_names(root: &Path) -> Result<Vec<String>, Error> {
    let dir = root.join("project");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(format!("reading {}", dir.display()), e)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(format!("reading {}", dir.display()), e))?;
        if let Some(name) = entry.file_name().to_str()
            && is_project_name(name)
            && entry.path().is_dir()
        {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_s_name_is_lower_case_and_its_root_is_under_project() {
        let root = Path::new("/home");
        assert_eq!(
            project_root(root, "demo-1.x_y").unwrap(),
            Path::new("/home/project/demo-1.x_y")
        );
        for name in ["Demo", "", ".hidden", "-x", "a/b", ".."] {
            assert!(
                matches!(project_root(root, name), Err(Error::InvalidName(_))),
                "{name:?}"
            );
        }
    }
}
