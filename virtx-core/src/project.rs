//! Projects: what everything a server keeps belongs to, named by each request in
//! [`PROJECT_HEADER`], as the Projects section of `PROTOCOL.md` has it.

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
