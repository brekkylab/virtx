//! Projects with members: each is named by its owner, and has members, each in a role, who
//! alone reach it. [`Membership`] says which project an owner names so and who is a member of
//! each; a server keeps it as it chooses, in files beside each project's directory
//! ([`TcpMembership`]) or in a database, and [`Access`](crate::access::Access) asks it of every
//! request.

use std::collections::BTreeMap;
use std::path::PathBuf;

use futures_util::future::BoxFuture;
use virtx_core::{DEFAULT_PROJECT, Role};
pub use virtx_uvm::{Project, ProjectOwner as Owner};
use virtx_uvm::{ProjectError, ProjectId, ProjectStore};

type Result<T> = std::result::Result<T, ProjectError>;

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

/// The projects, each kept as its [`ProjectStore`] record, beside those of a `virtx-stdio` over
/// the same root, which are the admin's and no user's to reach.
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
}

impl Membership for TcpMembership {
    fn resolve<'a>(
        &'a self,
        owner: &'a Owner,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProjectId>>> {
        Box::pin(async move {
            if let Some(project) = self.store.find(owner, name)? {
                return Ok(Some(project.id));
            }
            if name != DEFAULT_PROJECT {
                return Ok(None);
            }
            let members = owner.user().map(|user| (user.to_owned(), Role::Owner));
            match self
                .store
                .create(owner, name, members.into_iter().collect())
            {
                Ok(project) => Ok(Some(project.id)),
                // Made by a request beside this one.
                Err(ProjectError::AlreadyExists(_)) => {
                    Ok(self.store.find(owner, name)?.map(|p| p.id))
                }
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
            let members = BTreeMap::from([(creator.to_owned(), Role::Owner)]);
            self.store.create(owner, name, members)
        })
    }

    fn projects<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<Vec<Project>>> {
        Box::pin(async move {
            let mut reached = self.store.records()?;
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
            let mut project = self
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
            self.store.write(&project)
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

    /// A `virtx-stdio` over the same root keeps the admin's projects beside the users': read
    /// as any other, and reached by no user, whose own is their own whatever the admin's is.
    #[tokio::test]
    async fn the_admin_s_projects_are_beside_the_users_and_no_user_s() {
        let root = tempfile::tempdir().unwrap();
        let admin_s = ProjectStore::new(root.path())
            .create(&Owner::Admin, DEFAULT_PROJECT, BTreeMap::new())
            .unwrap();
        let store = TcpMembership::new(root.path());
        let alice = Owner::User("alice".into());

        let own = store
            .resolve(&alice, DEFAULT_PROJECT)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(own, admin_s.id);
        let listed: Vec<_> = store
            .projects("alice")
            .await
            .unwrap()
            .into_iter()
            .map(|project| project.id)
            .collect();
        assert_eq!(listed, [own]);
        assert_eq!(store.project(&admin_s.id).await.unwrap(), Some(admin_s));
    }
}
