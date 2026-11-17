//! Projects with members: each is named by its owner, and has members, each in a role, who
//! alone reach it. [`Membership`] says which project an owner names so and who is a member of
//! each; a server keeps it as it chooses, in files beside each project's directory
//! ([`TcpMembership`]) or in a database, and [`Access`](crate::access::Access) asks it of every
//! request.

use std::collections::BTreeMap;
use std::path::PathBuf;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use virtx_core::{DEFAULT_PROJECT, Role};
use virtx_uvm::{ProjectError, ProjectId, ProjectStore};

type Result<T> = std::result::Result<T, ProjectError>;

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

/// Which project an owner names so, and who is a member of each, in which role.
///
/// A project is forgotten ([`remove`](Membership::remove)) before its directory is deleted, so
/// that no request reaches what is being deleted; a directory left by a removal that did not
/// end is no project's, and reached by no request, since no id is given twice.
pub trait Membership: Send + Sync {
    /// The project `owner` names `name`, if any. A user's [`DEFAULT_PROJECT`] is always there:
    /// made, the user its owner, as it is first resolved.
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

/// The projects, each kept as its [`ProjectStore`] record, the [`Project`] with its members.
pub struct TcpMembership {
    store: ProjectStore,
}

impl TcpMembership {
    /// The projects kept under `root`.
    pub fn new(root: impl Into<PathBuf>) -> TcpMembership {
        TcpMembership {
            store: ProjectStore::new(root),
        }
    }

    /// The project `owner` names `name`, if any, as kept.
    fn find(&self, owner: &Owner, name: &str) -> Result<Option<Project>> {
        let all = self.store.records::<Project>()?;
        Ok(all
            .into_iter()
            .find(|project| project.owner == *owner && project.name == name))
    }

    /// Make project `name` of `owner`'s, `creator` its owner, holding the lock.
    fn make(&self, owner: &Owner, name: &str, creator: &str) -> Result<Project> {
        let _lock = self.store.lock()?;
        if self.find(owner, name)?.is_some() {
            return Err(ProjectError::AlreadyExists(name.to_owned()));
        }
        let project = Project {
            id: ProjectId::new(),
            owner: owner.clone(),
            name: name.to_owned(),
            members: BTreeMap::from([(creator.to_owned(), Role::Owner)]),
        };
        self.store.write(&project.id, &project)?;
        Ok(project)
    }
}

impl Membership for TcpMembership {
    fn resolve<'a>(
        &'a self,
        owner: &'a Owner,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProjectId>>> {
        Box::pin(async move {
            if let Some(project) = self.find(owner, name)? {
                return Ok(Some(project.id));
            }
            if name != DEFAULT_PROJECT {
                return Ok(None);
            }
            let Owner::User(user) = owner;
            match self.make(owner, name, user) {
                Ok(project) => Ok(Some(project.id)),
                // Made by a request beside this one.
                Err(ProjectError::AlreadyExists(_)) => Ok(self.find(owner, name)?.map(|p| p.id)),
                Err(e) => Err(e),
            }
        })
    }

    fn project<'a>(&'a self, id: &'a ProjectId) -> BoxFuture<'a, Result<Option<Project>>> {
        Box::pin(async move { self.store.read(id) })
    }

    fn create<'a>(
        &'a self,
        owner: &'a Owner,
        name: &'a str,
        creator: &'a str,
    ) -> BoxFuture<'a, Result<Project>> {
        Box::pin(async move {
            if !virtx_uvm::is_project_name(name) {
                return Err(ProjectError::InvalidName(name.to_owned()));
            }
            self.make(owner, name, creator)
        })
    }

    fn projects<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<Vec<Project>>> {
        Box::pin(async move {
            let mut reached = self.store.records::<Project>()?;
            reached.retain(|project| project.members.contains_key(user));
            reached.sort_by(|a, b| (&a.name, &a.owner).cmp(&(&b.name, &b.owner)));
            Ok(reached)
        })
    }

    fn set_member<'a>(
        &'a self,
        id: &'a ProjectId,
        user: &'a str,
        role: Option<Role>,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let _lock = self.store.lock()?;
            let mut project: Project = self
                .store
                .read(id)?
                .ok_or_else(|| ProjectError::Unknown(id.to_string()))?;
            if project.owner == Owner::User(user.to_owned()) && role != Some(Role::Owner) {
                return Err(ProjectError::NoOwner(id.to_string()));
            }
            match role {
                Some(role) => project.members.insert(user.to_owned(), role),
                None => project.members.remove(user),
            };
            self.store.write(id, &project)
        })
    }

    fn remove<'a>(&'a self, id: &'a ProjectId) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let _lock = self.store.lock()?;
            self.store.forget(id)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[tokio::test]
    async fn an_owner_names_each_project_once_and_its_members_reach_it_in_their_roles() {
        let root = tempfile::tempdir().unwrap();
        let store = TcpMembership::new(root.path());
        let (alice, bob) = (Owner::User("alice".into()), Owner::User("bob".into()));

        assert_eq!(store.resolve(&alice, "demo").await.unwrap(), None);
        let demo = store.create(&alice, "demo", "alice").await.unwrap();
        assert!(
            root.path()
                .join("project")
                .join(demo.id.to_string())
                .join("project.json")
                .is_file()
        );
        assert_eq!(store.resolve(&alice, "demo").await.unwrap(), Some(demo.id));
        assert!(matches!(
            store.create(&alice, "demo", "alice").await,
            Err(ProjectError::AlreadyExists(_))
        ));
        assert!(matches!(
            store.create(&alice, "Demo", "alice").await,
            Err(ProjectError::InvalidName(_))
        ));
        // Another owner's of the same name is another project.
        let theirs = store.create(&bob, "demo", "bob").await.unwrap();
        assert_ne!(theirs.id, demo.id);
        assert_eq!(store.resolve(&bob, "demo").await.unwrap(), Some(theirs.id));
        assert_eq!(store.project(&demo.id).await.unwrap(), Some(demo.clone()));

        store
            .set_member(&demo.id, "bob", Some(Role::Member))
            .await
            .unwrap();
        let listed: Vec<_> = store
            .projects("bob")
            .await
            .unwrap()
            .into_iter()
            .map(|project| (project.owner, project.members["bob"]))
            .collect();
        assert_eq!(listed, [(alice.clone(), Role::Member), (bob, Role::Owner)]);

        // Its owner neither leaves nor steps down; another owner leaves.
        for role in [None, Some(Role::Member)] {
            assert!(matches!(
                store.set_member(&demo.id, "alice", role).await,
                Err(ProjectError::NoOwner(_))
            ));
        }
        store
            .set_member(&demo.id, "bob", Some(Role::Owner))
            .await
            .unwrap();
        store.set_member(&demo.id, "bob", None).await.unwrap();

        // Kept on disk, for the next membership over the same root.
        let again = TcpMembership::new(root.path());
        let kept = again.project(&demo.id).await.unwrap().unwrap();
        assert_eq!(
            kept.members.into_iter().collect::<Vec<_>>(),
            [("alice".to_owned(), Role::Owner)]
        );

        again.remove(&demo.id).await.unwrap();
        assert_eq!(again.project(&demo.id).await.unwrap(), None);
        assert_eq!(again.resolve(&alice, "demo").await.unwrap(), None);
        assert!(matches!(
            again.remove(&demo.id).await,
            Err(ProjectError::Unknown(_))
        ));
        // Its name is its owner's to give again, to a project of another id.
        let anew = again.create(&alice, "demo", "alice").await.unwrap();
        assert_ne!(anew.id, demo.id);
    }

    /// A create that failed writing leaves a directory with no `project.json`: no project, and
    /// no name taken, so the user's own and a project of that name are made after it.
    #[tokio::test]
    async fn a_create_that_did_not_end_takes_no_name() {
        let root = tempfile::tempdir().unwrap();
        let store = TcpMembership::new(root.path());
        let alice = Owner::User("alice".into());
        let left = ProjectId::new();
        let dir = ProjectStore::new(root.path()).path(&left);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("project.json.part"), "{").unwrap();

        assert_eq!(store.project(&left).await.unwrap(), None);
        assert_eq!(store.projects("alice").await.unwrap(), []);
        let own = store
            .resolve(&alice, DEFAULT_PROJECT)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(own, left);
        let kept = store.project(&own).await.unwrap().unwrap();
        assert_eq!(kept.members.get("alice"), Some(&Role::Owner));
        let demo = store.create(&alice, "demo", "alice").await.unwrap();
        assert_ne!(demo.id, left);
    }
}
