//! Projects with members: each is named by its owner, and reached by whom it gives a role.
//! [`Membership`] authorizes every request, from who [`Identity`](crate::access::Identity)
//! says sent it: which project they name so, and their role in it. A server decides it as it
//! chooses, from files beside each project's directory ([`TcpMembership`]), a database, or the
//! claims a token carries, and [`Access`](crate::access::Access) asks it of every request.

use std::collections::BTreeMap;
use std::path::PathBuf;

use futures_util::future::BoxFuture;
use virtx_core::{DEFAULT_PROJECT, Role};
pub use virtx_uvm::{Project, ProjectOwner as Owner};
use virtx_uvm::{ProjectError, ProjectId, ProjectStore};

use crate::access::Principal;

type Result<T> = std::result::Result<T, ProjectError>;

/// Which project who names so, and their role in each: every decision about who reaches a
/// project is one of these.
///
/// A project is forgotten ([`remove`](Membership::remove)) before its directory is deleted, so
/// that no request reaches what is being deleted; a directory left by a removal that did not
/// end is no project's, and reached by no request, since no id is given twice.
pub trait Membership: Send + Sync {
    /// The project `who` names `name`, if any. Their own, [`DEFAULT_PROJECT`], is always there:
    /// made as it is first resolved, where projects are made here.
    fn resolve<'a>(
        &'a self,
        who: &'a Principal,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProjectId>>>;

    /// `who`'s role in project `id`: `None` where they have none, or there is no such project.
    fn role<'a>(
        &'a self,
        id: &'a ProjectId,
        who: &'a Principal,
    ) -> BoxFuture<'a, Result<Option<Role>>>;

    /// Project `id`, with its members, if there is one.
    fn project<'a>(&'a self, id: &'a ProjectId) -> BoxFuture<'a, Result<Option<Project>>>;

    /// Make project `name` of `who`'s, under an id no project has had, with them its one member,
    /// an owner: refused where they have a project of that name.
    fn create<'a>(&'a self, who: &'a Principal, name: &'a str) -> BoxFuture<'a, Result<Project>>;

    /// Every project `who` has a role in, with that role, by name.
    fn projects<'a>(&'a self, who: &'a Principal) -> BoxFuture<'a, Result<Vec<(Project, Role)>>>;

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

/// The projects, each kept as its [`ProjectStore`] record, its members in it: a user reaches a
/// project in the role its record gives them. Beside them are those of a `virtx-stdio` over the
/// same root, which are the admin's and no user's to reach.
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
        who: &'a Principal,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProjectId>>> {
        Box::pin(async move {
            let owner = Owner::User(who.user.clone());
            if let Some(project) = self.store.find(&owner, name)? {
                return Ok(Some(project.id));
            }
            if name != DEFAULT_PROJECT {
                return Ok(None);
            }
            let members = BTreeMap::from([(who.user.clone(), Role::Owner)]);
            match self.store.create(&owner, name, members) {
                Ok(project) => Ok(Some(project.id)),
                // Made by a request beside this one.
                Err(ProjectError::AlreadyExists(_)) => {
                    Ok(self.store.find(&owner, name)?.map(|p| p.id))
                }
                Err(e) => Err(e),
            }
        })
    }

    fn role<'a>(
        &'a self,
        id: &'a ProjectId,
        who: &'a Principal,
    ) -> BoxFuture<'a, Result<Option<Role>>> {
        Box::pin(async move {
            let project = self.store.read(id)?;
            Ok(project.and_then(|project| project.members.get(&who.user).copied()))
        })
    }

    fn project<'a>(&'a self, id: &'a ProjectId) -> BoxFuture<'a, Result<Option<Project>>> {
        Box::pin(async move { self.store.read(id) })
    }

    fn create<'a>(&'a self, who: &'a Principal, name: &'a str) -> BoxFuture<'a, Result<Project>> {
        Box::pin(async move {
            let owner = Owner::User(who.user.clone());
            let members = BTreeMap::from([(who.user.clone(), Role::Owner)]);
            self.store.create(&owner, name, members)
        })
    }

    fn projects<'a>(&'a self, who: &'a Principal) -> BoxFuture<'a, Result<Vec<(Project, Role)>>> {
        Box::pin(async move {
            let mut reached: Vec<_> = self
                .store
                .records()?
                .into_iter()
                .filter_map(|project| {
                    let role = *project.members.get(&who.user)?;
                    Some((project, role))
                })
                .collect();
            reached.sort_by(|(a, _), (b, _)| (&a.name, &a.owner).cmp(&(&b.name, &b.owner)));
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
        let (alice, bob) = (Principal::new("alice"), Principal::new("bob"));

        assert_eq!(store.resolve(&alice, "demo").await.unwrap(), None);
        let demo = store.create(&alice, "demo").await.unwrap();
        assert!(
            root.path()
                .join("project")
                .join(demo.id.to_string())
                .join("project.json")
                .is_file()
        );
        assert_eq!(store.resolve(&alice, "demo").await.unwrap(), Some(demo.id));
        assert!(matches!(
            store.create(&alice, "demo").await,
            Err(ProjectError::AlreadyExists(_))
        ));
        assert!(matches!(
            store.create(&alice, "Demo").await,
            Err(ProjectError::InvalidName(_))
        ));
        // Another owner's of the same name is another project.
        let theirs = store.create(&bob, "demo").await.unwrap();
        assert_ne!(theirs.id, demo.id);
        assert_eq!(store.resolve(&bob, "demo").await.unwrap(), Some(theirs.id));
        assert_eq!(store.project(&demo.id).await.unwrap(), Some(demo.clone()));
        assert_eq!(
            store.role(&demo.id, &alice).await.unwrap(),
            Some(Role::Owner)
        );
        assert_eq!(store.role(&demo.id, &bob).await.unwrap(), None);

        store
            .set_member(&demo.id, "bob", Some(Role::Member))
            .await
            .unwrap();
        assert_eq!(
            store.role(&demo.id, &bob).await.unwrap(),
            Some(Role::Member)
        );
        let listed: Vec<_> = store
            .projects(&bob)
            .await
            .unwrap()
            .into_iter()
            .map(|(project, role)| (project.owner, role))
            .collect();
        assert_eq!(
            listed,
            [
                (Owner::User("alice".into()), Role::Member),
                (Owner::User("bob".into()), Role::Owner)
            ]
        );

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
        let anew = again.create(&alice, "demo").await.unwrap();
        assert_ne!(anew.id, demo.id);
    }

    /// A create that failed writing leaves a directory with no `project.json`: no project, and
    /// no name taken, so the user's own and a project of that name are made after it.
    #[tokio::test]
    async fn a_create_that_did_not_end_takes_no_name() {
        let root = tempfile::tempdir().unwrap();
        let store = TcpMembership::new(root.path());
        let alice = Principal::new("alice");
        let left = ProjectId::new();
        let dir = ProjectStore::new(root.path()).path(&left);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("project.json.part"), "{").unwrap();

        assert_eq!(store.project(&left).await.unwrap(), None);
        assert_eq!(store.projects(&alice).await.unwrap(), []);
        let own = store
            .resolve(&alice, DEFAULT_PROJECT)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(own, left);
        let kept = store.project(&own).await.unwrap().unwrap();
        assert_eq!(kept.members.get("alice"), Some(&Role::Owner));
        let demo = store.create(&alice, "demo").await.unwrap();
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
        let alice = Principal::new("alice");

        let own = store
            .resolve(&alice, DEFAULT_PROJECT)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(own, admin_s.id);
        let listed: Vec<_> = store
            .projects(&alice)
            .await
            .unwrap()
            .into_iter()
            .map(|(project, _)| project.id)
            .collect();
        assert_eq!(listed, [own]);
        assert_eq!(store.role(&admin_s.id, &alice).await.unwrap(), None);
        assert_eq!(store.project(&admin_s.id).await.unwrap(), Some(admin_s));
    }
}
