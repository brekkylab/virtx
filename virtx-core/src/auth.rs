//! The credentials a client sends with every request to a server over HTTP, as the
//! Authentication section of `PROTOCOL.md` has a server want them.

use serde::{Deserialize, Serialize};

/// The credentials sent with every request: none, or a token sent as `Authorization: Bearer
/// <token>`. `Debug` leaves the token out.
///
/// As JSON, `{"type": "none"}` or `{"type": "bearer", "token": "..."}`, as a
/// [`RegistryAuth`](crate::RegistryAuth) is spelled.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Auth {
    #[default]
    None,
    Bearer {
        token: String,
    },
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Bearer { .. } => f.debug_struct("Bearer").finish_non_exhaustive(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_leaves_the_token_out() {
        let bearer = Auth::Bearer {
            token: "secret".into(),
        };
        assert!(!format!("{bearer:?}").contains("secret"));
        assert_eq!(
            serde_json::to_value(&bearer).unwrap(),
            serde_json::json!({"type": "bearer", "token": "secret"})
        );
    }
}
