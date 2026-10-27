//! Who may send a request, and which project's [`Home`] it reaches: every request carries a
//! bearer token and names its project, and is let through only for a member of it. A project's
//! sandboxes are its members' together.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use http::{HeaderMap, StatusCode, header};
use virtx_uvm::ImageStore;

use crate::config::{Config, hashed};
use crate::rpc::Home;

/// The header a request names its project in.
pub const PROJECT_HEADER: &str = "virtx-project";

pub struct Access {
    /// Each user, by the SHA-256 of their token.
    users: HashMap<String, String>,
    /// The `Origin`s a browser may call from.
    origins: Vec<String>,
    /// Each project's members, and its home.
    projects: HashMap<String, (HashSet<String>, Arc<Home>)>,
}

impl Access {
    /// The users and projects `config` names, each project's home made where it says.
    pub fn new(config: &Config) -> anyhow::Result<Access> {
        let root = config.home();
        let images = ImageStore::new(root.join("images"))?;
        let mut projects = HashMap::new();
        for (name, project) in &config.projects {
            let dir = root.join("project").join(name);
            let home = Home::new(images.clone(), &dir)?;
            let members = project.members.iter().cloned().collect();
            projects.insert(name.clone(), (members, Arc::new(home)));
        }
        Ok(Access {
            users: config.users(),
            origins: config.origins.clone(),
            projects,
        })
    }

    /// Every project's home.
    pub fn homes(&self) -> impl Iterator<Item = &Arc<Home>> {
        self.projects.values().map(|(_, home)| home)
    }

    /// The home a request with `headers` reaches: `403` from an `Origin` not let in, `401`
    /// without a token this server knows, `400` naming no project, and `403` naming one the
    /// user is not a member of -- or none at all, which is not told apart.
    pub fn home(&self, headers: &HeaderMap) -> Result<Arc<Home>, StatusCode> {
        if let Some(origin) = headers.get(header::ORIGIN)
            && !self
                .origins
                .iter()
                .any(|allowed| allowed.as_bytes() == origin.as_bytes())
        {
            return Err(StatusCode::FORBIDDEN);
        }
        let user = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .and_then(|token| self.users.get(&hashed(token)))
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let project = headers
            .get(PROJECT_HEADER)
            .and_then(|value| value.to_str().ok())
            .ok_or(StatusCode::BAD_REQUEST)?;
        self.projects
            .get(project)
            .filter(|(members, _)| members.contains(user))
            .map(|(_, home)| Arc::clone(home))
            .ok_or(StatusCode::FORBIDDEN)
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    fn access(home: &std::path::Path) -> Access {
        let config = format!(
            r#"{{
                "listen": "127.0.0.1:0",
                "home": {home:?},
                "origins": ["https://allowed.example"],
                "users": {{
                    "alice": {{ "token_sha256": "{}" }},
                    "bob": {{ "token_sha256": "{}" }}
                }},
                "projects": {{ "demo": {{ "members": ["alice"] }} }}
            }}"#,
            hashed("a-token"),
            hashed("b-token"),
        );
        let path = home.join("config.json");
        std::fs::write(&path, config).unwrap();
        Access::new(&Config::read(&path).unwrap()).unwrap()
    }

    fn headers(token: Option<&str>, project: Option<&str>, origin: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(token) = token {
            let value = HeaderValue::from_str(&format!("Bearer {token}")).unwrap();
            headers.insert(header::AUTHORIZATION, value);
        }
        if let Some(project) = project {
            headers.insert(PROJECT_HEADER, HeaderValue::from_str(project).unwrap());
        }
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
        }
        headers
    }

    #[test]
    fn a_member_reaches_the_project_s_home_and_no_one_else_does() {
        let dir = std::env::temp_dir().join(format!("virtx-http-access-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let access = access(&dir);
        let home = |token, project, origin| access.home(&headers(token, project, origin));

        let reached = home(Some("a-token"), Some("demo"), None).unwrap();
        assert_eq!(
            reached.snapshots.root(),
            dir.join("project").join("demo").as_path()
        );
        assert!(
            home(
                Some("a-token"),
                Some("demo"),
                Some("https://allowed.example")
            )
            .is_ok()
        );

        let refused = |token, project, origin| home(token, project, origin).err();
        assert_eq!(
            refused(Some("a-token"), Some("demo"), Some("https://evil.example")),
            Some(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            refused(None, Some("demo"), None),
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            refused(Some("x"), Some("demo"), None),
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            refused(Some("a-token"), None, None),
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            refused(Some("b-token"), Some("demo"), None),
            Some(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            refused(Some("a-token"), Some("other"), None),
            Some(StatusCode::FORBIDDEN)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
