//! Projects: what everything a server keeps belongs to. Each project is a directory of its own,
//! under an id given it once, which its images, sandboxes and volumes are kept in.
//! [`Store`] keeps the directories, and in each the server's record of the project, its name and
//! whatever else that server keeps of it, such as its members.
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   project/.lock             held while a record changes
//!   project/<id>/             a project's own: images/, sandbox/, volume/
//!   project/<id>/project.json its record: a project is there while this is
//! ```

use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::str::FromStr;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

    /// A server's own failure, such as its database's.
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

/// The projects' directories under a server's root, and the record of each in it.
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

    /// The lock held while a record changes, until it drops: servers over one root change them
    /// one at a time.
    pub fn lock(&self) -> Result<fs::File> {
        let dir = self.dir();
        fs::create_dir_all(&dir).map_err(|e| Error::io(format!("making {}", dir.display()), e))?;
        let path = dir.join(".lock");
        let lock = fs::File::create(&path)
            .map_err(|e| Error::io(format!("opening {}", path.display()), e))?;
        lock.lock()
            .map_err(|e| Error::io(format!("locking {}", path.display()), e))?;
        Ok(lock)
    }

    /// Project `id`'s record: `None` where it has none.
    pub fn read<T: DeserializeOwned>(&self, id: &ProjectId) -> Result<Option<T>> {
        let path = self.path(id).join("project.json");
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| Error::io(format!("reading {}", path.display()), e.into())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io(format!("reading {}", path.display()), e)),
        }
    }

    /// Every project's record, by id: a directory with none, left by a create that did not
    /// end, is no project.
    pub fn records<T: DeserializeOwned>(&self) -> Result<Vec<T>> {
        let mut all = Vec::new();
        for id in self.list()? {
            all.extend(self.read(&id)?);
        }
        Ok(all)
    }

    /// Keep `record` as project `id`'s, whole: written beside, and renamed over, so that the
    /// project is there once it is all written.
    pub fn write<T: Serialize>(&self, id: &ProjectId, record: &T) -> Result<()> {
        let dir = self.path(id);
        fs::create_dir_all(&dir).map_err(|e| Error::io(format!("making {}", dir.display()), e))?;
        let (path, part) = (dir.join("project.json"), dir.join("project.json.part"));
        let bytes = serde_json::to_vec_pretty(record).map_err(|e| Error::Other(e.into()))?;
        fs::write(&part, bytes).map_err(|e| Error::io(format!("writing {}", part.display()), e))?;
        fs::rename(&part, &path).map_err(|e| Error::io(format!("writing {}", path.display()), e))
    }

    /// Forget project `id`, its record removed, so that no request reaches it while its
    /// directory is deleted: a directory left by a removal that did not end is no project's.
    pub fn forget(&self, id: &ProjectId) -> Result<()> {
        let path = self.path(id).join("project.json");
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Error::Unknown(id.to_string())),
            Err(e) => Err(Error::io(format!("removing {}", path.display()), e)),
        }
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

    #[test]
    fn a_project_is_there_while_its_record_is() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path());
        let (kept, left) = (ProjectId::new(), ProjectId::new());
        store.write(&kept, &"demo").unwrap();
        // A create that did not end leaves a directory with no record.
        fs::create_dir_all(store.path(&left)).unwrap();
        fs::write(store.path(&left).join("project.json.part"), "{").unwrap();

        assert_eq!(
            store.read::<String>(&kept).unwrap().as_deref(),
            Some("demo")
        );
        assert_eq!(store.read::<String>(&left).unwrap(), None);
        assert_eq!(store.records::<String>().unwrap(), ["demo"]);

        store.forget(&kept).unwrap();
        assert_eq!(store.records::<String>().unwrap(), Vec::<String>::new());
        assert!(matches!(store.forget(&kept), Err(Error::Unknown(_))));
        assert!(store.path(&kept).is_dir());
    }
}
