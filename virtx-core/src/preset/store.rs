//! Presets: what one is, as the server answers it, what keeping one takes, and what making a
//! sandbox of one takes.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::sandbox::{Sandbox, SandboxCreateArgs, SandboxResizeArgs};

/// A preset, as the server answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preset {
    /// The id the server gave it, which names its project with it: what an app names it by.
    pub id: String,

    /// The name it is kept under in its project.
    pub name: String,

    /// What each sandbox made of it is made with, as a sandbox's `POST` takes it, with no name.
    pub sandbox: SandboxCreateArgs,

    /// How large a sandbox made of it may be resized with its grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<SandboxResizeArgs>,

    /// Who beside the project's members may make a sandbox of it.
    #[serde(default)]
    pub access: Vec<PresetAccess>,

    /// The origins a web page may make a sandbox of it from, and reach that sandbox from.
    #[serde(default)]
    pub origins: Vec<String>,

    /// How many sandboxes those `access` lets in may have of it at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<PresetLimits>,
}

/// What keeping a preset takes: `PUT /presets/{name}`'s body, which makes it or replaces it in
/// whole.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetPutArgs {
    /// What each sandbox made of it is made with, as a sandbox's `POST` takes it. A `name` is
    /// refused: each sandbox is named as it is made, if at all.
    pub sandbox: SandboxCreateArgs,

    /// How large a sandbox made of it may be resized with its grant, each no less than what
    /// `sandbox` makes it. One left out holds the grant to the size it was made with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<SandboxResizeArgs>,

    /// Who beside the project's members may make a sandbox of it, any one way letting a request
    /// in. Without any, its members alone may.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access: Vec<PresetAccess>,

    /// The origins a web page may make a sandbox of it from, and reach that sandbox with its
    /// grant from, beside those the server allows itself: each a scheme, host and port, as
    /// `https://app.example`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origins: Vec<String>,

    /// How many sandboxes made of it by those `access` lets in may live at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<PresetLimits>,
}

impl PresetPutArgs {
    /// A preset of `sandbox`, its members alone making it, with nothing else said. Each method
    /// after sets one field, so they chain.
    ///
    /// ```
    /// # use virtx_core::{PresetAccess, PresetPutArgs, SandboxCreateArgs};
    /// let args = PresetPutArgs::new(SandboxCreateArgs::new("app:latest").lifetime(3600))
    ///     .access(PresetAccess::Public)
    ///     .origin("https://app.example");
    ///
    /// assert_eq!(args.access, [PresetAccess::Public]);
    /// ```
    pub fn new(sandbox: SandboxCreateArgs) -> Self {
        Self {
            sandbox,
            max: None,
            access: Vec::new(),
            origins: Vec::new(),
            limits: None,
        }
    }

    pub fn max(mut self, max: SandboxResizeArgs) -> Self {
        self.max = Some(max);
        self
    }

    /// Let in whom `access` lets in, beside the ways given before.
    pub fn access(mut self, access: PresetAccess) -> Self {
        self.access.push(access);
        self
    }

    /// Let a web page at `origin` in, beside those given before.
    pub fn origin(mut self, origin: impl Into<String>) -> Self {
        self.origins.push(origin.into());
        self
    }

    pub fn limits(mut self, limits: PresetLimits) -> Self {
        self.limits = Some(limits);
        self
    }
}

/// A way in to make a sandbox of a preset, for one who is no member of its project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PresetAccess {
    /// A request with no credentials, from anyone: its subject is the address it came from.
    Public,

    /// A request whose bearer token is a JSON Web Token `issuer` signed for `audience`: its
    /// subject is the token's `sub`.
    Jwt {
        /// The issuer, as its tokens' `iss` has it: an `https` URL.
        issuer: String,

        /// What the tokens' `aud` is, or holds.
        audience: String,

        /// Where the issuer's keys are, as a JWK Set, for an issuer with no OpenID Connect
        /// discovery document.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        jwks: Option<String>,

        /// Claims the token carries, each equal to the value here.
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        claims: Map<String, Value>,
    },
}

/// How many sandboxes made of a preset by those its access lets in may live at once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetLimits {
    /// All of theirs together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandboxes: Option<u32>,

    /// Each subject's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_subject: Option<u32>,
}

/// What making a sandbox of a preset takes: `POST /presets/{preset}/sandboxes/`'s body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetSandboxCreateArgs {
    /// A name to find it again by, given by a member alone: one the preset's access lets in is
    /// refused one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A sandbox made of a preset, as `POST /presets/{preset}/sandboxes/` answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresetSandbox {
    /// The sandbox, as a sandbox's `POST` answers it.
    pub sandbox: Sandbox,

    /// The token that reaches the sandbox alone, given out in this answer and no other. A
    /// server that takes no credentials gives none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_way_in_is_tagged_by_its_type() {
        let jwt: PresetAccess = serde_json::from_value(json!({
            "type": "jwt",
            "issuer": "https://idp.example",
            "audience": "app",
            "claims": {"repository": "owner/app"},
        }))
        .unwrap();
        let PresetAccess::Jwt { jwks, claims, .. } = &jwt else {
            panic!("{jwt:?}");
        };
        assert_eq!((jwks, &claims["repository"]), (&None, &json!("owner/app")));
        assert_eq!(
            serde_json::to_value(PresetAccess::Public).unwrap(),
            json!({"type": "public"})
        );
        let unknown = json!({"type": "jwt", "issuer": "https://idp.example", "audience": "app",
            "subject": "alice"});
        assert!(serde_json::from_value::<PresetAccess>(unknown).is_err());
    }

    #[test]
    fn a_preset_is_read_past_members_it_does_not_know() {
        let value = json!({
            "id": "p", "name": "app", "sandbox": {"image": {"type": "tag", "tag": "app:1"}},
            "access": [], "origins": [], "cell": "c1",
        });
        let preset: Preset = serde_json::from_value(value).unwrap();
        assert_eq!(preset.sandbox, SandboxCreateArgs::new("app:1"));
    }
}
