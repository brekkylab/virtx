//! Writing a file so that its name only ever holds the whole of it.

use std::fs::{self, File};
use std::io;
use std::path::Path;

/// Write the file at `path` with `write`, and only then give it that name.
///
/// What is kept under a name here is taken as whole for being there: a layer that exists is
/// not fetched again, a root filesystem that exists is booted. So a write that stops
/// part-way -- an error, a full disk, a process killed -- must leave nothing under the name.
/// The bytes go to a file beside it first, and a rename, which is atomic within a
/// directory, moves them under the name once `write` has finished.
///
/// The file beside it is named for this process, so two writing the same name at once each
/// write their own and the last rename wins -- which, the name addressing what it holds, is
/// the same file. One `write` refused is removed; one a crash left is under a name nothing
/// looks up, and the next write of it truncates it.
///
/// The outer error is the file's: making its directory, creating it, renaming it. The
/// inner one is whatever `write` answered, and leaves nothing behind either.
pub(crate) fn write_atomically<T, E>(
    path: &Path,
    write: impl FnOnce(&mut File) -> Result<T, E>,
) -> io::Result<Result<T, E>> {
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let part = path.with_file_name(format!("{name}.{}.part", std::process::id()));
    let named = |what: &str, at: &Path, e: io::Error| {
        io::Error::new(e.kind(), format!("{what} {}: {e}", at.display()))
    };

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| named("making", dir, e))?;
    }
    let mut file = File::create(&part).map_err(|e| named("creating", &part, e))?;

    let written = write(&mut file);
    // Closed before anything touches the name: Windows renames and removes nothing that is
    // still open.
    drop(file);

    match written {
        Ok(value) => {
            fs::rename(&part, path).map_err(|e| {
                let _ = fs::remove_file(&part);
                named("moving into place", path, e)
            })?;
            Ok(Ok(value))
        }
        Err(e) => {
            let _ = fs::remove_file(&part);
            Ok(Err(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir()
                .join(format!("virtx-uvm-lib-file-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Scratch(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn listed(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_finished_write_is_under_its_name_and_nothing_else() {
        let scratch = Scratch::new("finished");
        let path = scratch.0.join("deep").join("done");

        write_atomically(&path, |file| file.write_all(b"whole"))
            .unwrap()
            .unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"whole");
        assert_eq!(listed(path.parent().unwrap()), ["done"]);
    }

    #[test]
    fn a_refused_write_leaves_nothing() {
        let scratch = Scratch::new("refused");
        let path = scratch.0.join("never");

        let refused = write_atomically(&path, |file| {
            file.write_all(b"half").unwrap();
            Err::<(), _>("refused")
        })
        .unwrap();

        assert_eq!(refused, Err("refused"));
        assert!(listed(&scratch.0).is_empty(), "{:?}", listed(&scratch.0));
    }

    /// What was there stays until the new one is whole, and is then replaced by it.
    #[test]
    fn a_name_is_replaced_whole() {
        let scratch = Scratch::new("replaced");
        let path = scratch.0.join("file");
        write_atomically(&path, |file| file.write_all(b"old"))
            .unwrap()
            .unwrap();

        let _ = write_atomically(&path, |file| {
            file.write_all(b"ne").unwrap();
            Err::<(), _>(())
        });
        assert_eq!(fs::read(&path).unwrap(), b"old");

        write_atomically(&path, |file| file.write_all(b"new"))
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }
}
