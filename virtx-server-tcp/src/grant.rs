//! Grants: each a token that reaches one sandbox and nothing else, given out as the sandbox is
//! made of a preset. A home keeps each by its token's hash alone, with what it reaches, and
//! takes it away with its sandbox; the sandbox knows nothing of it.
//!
//! A grant's token names its project, so a request with one is taken to its home without
//! asking anyone: `vxg.<project>.<secret>`.
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   grant/<hash>.json   a grant, by the SHA-256 of its token, in hex
//! ```

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use virtx_core::SandboxResizeArgs;
use virtx_uvm::ProjectId;

const GRANT: &str = "grant";

/// What a grant's token starts with.
const PREFIX: &str = "vxg.";

/// What a grant reaches, and who it was given to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// The sandbox it reaches.
    pub sandbox: String,
    /// The preset the sandbox was made of, by id.
    pub preset: String,
    /// Who made it: the member, or the subject the preset's access let in.
    pub subject: String,
    /// Whether that was a member, who counts toward none of the preset's limits.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub member: bool,
    /// How large it may resize the sandbox to, each member given.
    pub max: SandboxResizeArgs,
    /// The origins a web page may send it from, beside the server's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origins: Vec<String>,
}

impl Grant {
    /// Whether it reaches a request to `path` with `method`: the listing of the sandboxes, which
    /// answers its own alone, its sandbox's `GET`, `PATCH` and `DELETE`, and everything under
    /// its sandbox.
    pub fn reaches(&self, method: &http::Method, path: &str) -> bool {
        use http::Method;
        let Some(rest) = path.strip_prefix("/sandboxes/") else {
            return false;
        };
        match rest.split_once('/') {
            None if rest.is_empty() => method == Method::GET,
            None => {
                rest == self.sandbox
                    && [Method::GET, Method::PATCH, Method::DELETE].contains(method)
            }
            Some((id, _)) => id == self.sandbox,
        }
    }
}

/// The project a grant's `token` names, if it is a grant's.
pub fn project_of(token: &str) -> Option<ProjectId> {
    let (project, _) = token.strip_prefix(PREFIX)?.split_once('.')?;
    project.parse().ok()
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

    /// Give out `grant` of project `project`: its token, which is kept nowhere.
    pub fn give(&self, project: &ProjectId, grant: &Grant) -> io::Result<String> {
        let secret = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let token = format!("{PREFIX}{project}.{secret}");
        let json = serde_json::to_vec_pretty(grant).map_err(io::Error::other)?;
        let path = self.path(&token);
        let part = path.with_extension("json.part");
        fs::write(&part, json)?;
        fs::rename(&part, &path)?;
        Ok(token)
    }

    /// The grant `token` is, if it is one here.
    pub fn find(&self, token: &str) -> io::Result<Option<Grant>> {
        match fs::read(self.path(token)) {
            Ok(json) => Ok(Some(
                serde_json::from_slice(&json).map_err(io::Error::other)?,
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Every grant here.
    pub fn list(&self) -> io::Result<Vec<Grant>> {
        let mut grants = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                grants.push(serde_json::from_slice(&fs::read(&path)?).map_err(io::Error::other)?);
            }
        }
        Ok(grants)
    }

    /// Take away every grant of sandbox `sandbox`.
    pub fn take_away(&self, sandbox: &str) -> io::Result<()> {
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let grant: Grant =
                serde_json::from_slice(&fs::read(&path)?).map_err(io::Error::other)?;
            if grant.sandbox == sandbox {
                fs::remove_file(&path)?;
            }
        }
        Ok(())
    }

    fn path(&self, token: &str) -> PathBuf {
        let hash = Sha256::digest(token.as_bytes());
        let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
        self.dir.join(format!("{hex}.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(sandbox: &str) -> Grant {
        Grant {
            sandbox: sandbox.into(),
            preset: "p".into(),
            subject: "alice".into(),
            member: false,
            max: SandboxResizeArgs::default().vcpus(2),
            origins: Vec::new(),
        }
    }

    /// A grant is found by its token alone, which names its project and is kept nowhere, and is
    /// taken away with its sandbox.
    #[test]
    fn a_grant_is_found_by_its_token_and_taken_away_with_its_sandbox() {
        let root = tempfile::tempdir().unwrap();
        let grants = Grants::new(root.path()).unwrap();
        let project = ProjectId::new();
        let token = grants.give(&project, &grant("a")).unwrap();
        let other = grants.give(&project, &grant("b")).unwrap();
        assert_eq!(project_of(&token), Some(project));
        assert_eq!(grants.find(&token).unwrap(), Some(grant("a")));
        assert_eq!(grants.find(&format!("{token}x")).unwrap(), None);
        assert_eq!(project_of("vxg.nothing.secret"), None);
        assert_eq!(project_of("eyJhbGciOi"), None);
        let kept = fs::read_dir(root.path().join(GRANT)).unwrap();
        for entry in kept {
            let json = fs::read_to_string(entry.unwrap().path()).unwrap();
            assert!(!json.contains(token.rsplit('.').next().unwrap()), "{json}");
        }

        grants.take_away("a").unwrap();
        assert_eq!(grants.find(&token).unwrap(), None);
        assert_eq!(grants.find(&other).unwrap(), Some(grant("b")));
        assert_eq!(grants.list().unwrap(), [grant("b")]);
    }
}
