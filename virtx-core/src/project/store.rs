//! Projects, and the members of each, as the server answers them and what making and changing
//! them takes.

use serde::{Deserialize, Serialize};

/// The header a request names its project in.
pub const PROJECT_HEADER: &str = "virtx-project";

/// The project a request that names none belongs to.
pub const DEFAULT_PROJECT: &str = "default";

/// What a member of a project may do there: an `Owner` all a `Member` may, and more, so it
/// orders above it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Reaches everything in the project, reads it and its members, and leaves it.
    Member,
    /// Also adds, removes and changes members, and deletes the project.
    Owner,
}

/// A project, as the server answers it, with the role of whoever asked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectListOutputItem {
    pub name: String,
    pub role: Role,
}

/// What making a project takes: `POST /projects/`'s body. Its maker is its owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectCreateArgs {
    pub name: String,
}

/// A member of a project, as the server answers it: the user as the server names them, such as
/// an email address, and their role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMemberListOutputItem {
    pub user: String,
    pub role: Role,
}

/// What giving a user a role in a project takes: `PUT /projects/{name}/members/{user}`'s body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMemberSetArgs {
    pub role: Role,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_is_spelled_in_lower_case_and_an_owner_outranks_a_member() {
        assert_eq!(serde_json::to_value(Role::Owner).unwrap(), "owner");
        assert_eq!(
            serde_json::from_value::<Role>("member".into()).unwrap(),
            Role::Member
        );
        assert!(Role::Owner > Role::Member);
    }
}
