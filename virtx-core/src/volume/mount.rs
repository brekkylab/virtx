use std::fmt;

use serde::{Deserialize, Serialize};

/// What a mount shows, and the credentials to reach it.
///
/// A refresh token is taken, not minted: the consent round trip belongs to whatever set the
/// mount up. Where each service is reached is the server's own setting, never a request's,
/// since a token endpoint is sent the client secret.
///
/// As JSON:
///
/// ```text
/// {"type": "notion", "api_key": "..."}
/// {"type": "gdrive", "client_id": "...", "client_secret": "...", "refresh_token": "..."}
/// {"type": "onedrive", "client_id": "...", "refresh_token": "..."}
/// ```
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MountSource {
    /// The pages a Notion integration can reach.
    Notion { api_key: String },

    /// A Google Drive.
    Gdrive {
        client_id: String,
        client_secret: String,
        refresh_token: String,
    },

    /// A OneDrive. `client_secret` is for a confidential app; a public one, as a personal
    /// account's usually is, has none.
    Onedrive {
        client_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_secret: Option<String>,
        refresh_token: String,
    },
}

impl MountSource {
    pub fn mount_type(&self) -> MountType {
        match self {
            Self::Notion { .. } => MountType::Notion,
            Self::Gdrive { .. } => MountType::Gdrive,
            Self::Onedrive { .. } => MountType::Onedrive,
        }
    }
}

/// Only the type: a credential has no place in a log.
impl fmt::Debug for MountSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MountSource")
            .field("type", &self.mount_type())
            .field("credentials", &"[redacted]")
            .finish()
    }
}

/// A [`MountSource`]'s type, without its credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountType {
    Notion,
    Gdrive,
    Onedrive,
}
