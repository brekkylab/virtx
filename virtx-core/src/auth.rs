//! The credentials a client sends with every request to a server over HTTP, as the
//! Authentication section of `PROTOCOL.md` has a server want them.

use serde::{Deserialize, Serialize};

/// The credentials sent with every request: none, or a token sent as `Authorization: Bearer
/// <token>`, given or printed by a command. `Debug` leaves the token out.
///
/// As JSON, `{"type": "none"}`, `{"type": "bearer", "token": "..."}` or `{"type": "command",
/// "program": "...", "args": [...]}`, as a [`RegistryAuth`](crate::RegistryAuth) is spelled.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Auth {
    #[default]
    None,
    Bearer {
        token: String,
    },
    /// The token `program` with `args` prints: a token alone, or `{"token": "...",
    /// "expires_in": <seconds>}`. It is run for the first request, and again when its token
    /// is a minute from running out or the server refuses it: a person's short-lived token,
    /// such as `virtx token --json` prints.
    Command {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Bearer { .. } => f.debug_struct("Bearer").finish_non_exhaustive(),
            Self::Command { program, args } => f
                .debug_struct("Command")
                .field("program", program)
                .field("args", args)
                .finish(),
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

    #[test]
    fn a_command_reads_without_its_args() {
        let command: Auth =
            serde_json::from_value(serde_json::json!({"type": "command", "program": "virtx"}))
                .unwrap();
        assert_eq!(
            command,
            Auth::Command {
                program: "virtx".into(),
                args: vec![]
            }
        );
    }
}
