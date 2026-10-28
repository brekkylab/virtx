use serde::{Deserialize, Serialize};

/// Credentials for a remote image registry. `Debug` leaves the secret out.
///
/// As JSON:
///
/// ```text
/// {"type": "basic", "username": "...", "password": "..."}
/// {"type": "bearer", "token": "..."}
/// ```
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RegistryAuth {
    /// A username, and a password or an access token the registry takes in its place.
    Basic { username: String, password: String },

    /// A token the registry takes as is.
    Bearer { token: String },
}

impl std::fmt::Debug for RegistryAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .finish_non_exhaustive(),
            Self::Bearer { .. } => f.debug_struct("Bearer").finish_non_exhaustive(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_leaves_the_secret_out() {
        let basic = RegistryAuth::Basic {
            username: "me".into(),
            password: "secret".into(),
        };
        let bearer = RegistryAuth::Bearer {
            token: "secret".into(),
        };
        assert!(!format!("{basic:?}").contains("secret"));
        assert!(!format!("{bearer:?}").contains("secret"));
    }
}
