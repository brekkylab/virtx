//! A preset, and the client its requests go through.

use virtx_core::Preset as PresetInfo;

use crate::{Call, Client, Sandbox};

/// A preset the server keeps, as keeping it or asking for it answered, and the client that
/// reaches it: what makes sandboxes of it.
///
/// Cloning holds the same client once more. Dropping it leaves the preset as it is; it goes
/// with [`remove`](Self::remove).
#[derive(Clone)]
pub struct Preset {
    client: Client,
    info: PresetInfo,
}

/// A sandbox made of a preset, and its grant: the token that reaches it alone, given out as it
/// is made and never again, which an [`AppClient`](crate::AppClient) reaches it with. A server
/// over stdio gives none.
#[derive(Clone, Debug)]
pub struct PresetSandbox {
    pub sandbox: Sandbox,
    pub grant: Option<String>,
}

impl Preset {
    pub(crate) fn new(client: Client, info: PresetInfo) -> Preset {
        Preset { client, info }
    }

    /// The client its requests go through.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The id the server gave it, which names its project with it: what an app names it by.
    pub fn id(&self) -> &str {
        &self.info.id
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }

    /// What the server answered about it: the sandbox it makes, and who else may make one.
    pub fn info(&self) -> &PresetInfo {
        &self.info
    }

    /// Make a sandbox of it, named `name` where one is given.
    pub fn create_sandbox(&self, name: Option<&str>) -> Call<PresetSandbox> {
        self.client.create_preset_sandbox(&self.info.id, name)
    }

    /// Remove it. The sandboxes made of it stay, each with its grant.
    pub fn remove(&self) -> Call<()> {
        self.client.remove_preset(&self.info.id)
    }
}

impl std::fmt::Debug for Preset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Preset").field(&self.info).finish()
    }
}
