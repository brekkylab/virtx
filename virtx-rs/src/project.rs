//! A project, and the client its requests go through.

use virtx_core::{
    ProjectListOutputItem as ProjectInfo, ProjectMemberListOutputItem as ProjectMemberInfo, Role,
};

use crate::{Call, Client};

/// A project the client has a role in, as making it or asking for it answered, and the client
/// that reaches it: its members, and its removal. What is in it, a client reaches with the
/// project set ([`Client::set_project`]).
///
/// Cloning holds the same client once more. Dropping it leaves the project as it is; it goes
/// with [`remove`](Self::remove).
#[derive(Clone)]
pub struct Project {
    client: Client,
    info: ProjectInfo,
}

impl Project {
    pub(crate) fn new(client: Client, info: ProjectInfo) -> Project {
        Project { client, info }
    }

    /// The client its requests go through.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// What names it on any server, given it once.
    pub fn id(&self) -> &str {
        &self.info.id
    }

    /// What its owner names it, among their projects.
    pub fn name(&self) -> &str {
        &self.info.name
    }

    /// The user whose project it is: none from a server with no users, such as over stdio.
    pub fn owner(&self) -> Option<&str> {
        self.info.owner.as_deref()
    }

    /// The client's role in it, as the server answered it.
    pub fn role(&self) -> Role {
        self.info.role
    }

    /// What the server answered about it.
    pub fn info(&self) -> &ProjectInfo {
        &self.info
    }

    /// Its members, with their roles.
    pub fn members(&self) -> Call<Vec<ProjectMemberInfo>> {
        self.client.list_project_member(&self.info.id)
    }

    /// Give `user` `role` in it, adding them if they are no member.
    pub fn set_member(&self, user: impl AsRef<str>, role: Role) -> Call<ProjectMemberInfo> {
        self.client.set_project_member(&self.info.id, user, role)
    }

    /// Take `user`'s role in it away: theirs to do, leaving it, or an owner's.
    pub fn remove_member(&self, user: impl AsRef<str>) -> Call<()> {
        self.client.remove_project_member(&self.info.id, user)
    }

    /// Delete it and everything in it.
    pub fn remove(&self) -> Call<()> {
        self.client.remove_project(&self.info.id)
    }
}

impl std::fmt::Debug for Project {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Project").field(&self.info).finish()
    }
}
