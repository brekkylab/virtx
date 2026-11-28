//! A package, and the client its requests go through.

use virtx_core::{Package as PackageInfo, PackageSandboxCreateArgs};

use crate::{Call, Client, Sandbox};

/// A package the server keeps, as keeping it or asking for it answered, and the client that
/// reaches it: what makes sandboxes of it.
///
/// Cloning holds the same client once more. Dropping it leaves the package as it is; it goes
/// with [`remove`](Self::remove).
#[derive(Clone)]
pub struct Package {
    client: Client,
    info: PackageInfo,
}

/// A sandbox made of a package, and a grant to it: the token that reaches it alone, given out in
/// this answer and no other, which an [`AppClient`](crate::AppClient) reaches it with. A server
/// over stdio gives none.
#[derive(Clone, Debug)]
pub struct PackageSandbox {
    pub sandbox: Sandbox,
    pub grant: Option<String>,
}

impl Package {
    pub(crate) fn new(client: Client, info: PackageInfo) -> Package {
        Package { client, info }
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
    pub fn info(&self) -> &PackageInfo {
        &self.info
    }

    /// Make a sandbox of it as `args` says: a name, and its ports on local.
    pub fn create_sandbox(&self, args: &PackageSandboxCreateArgs) -> Call<PackageSandbox> {
        self.client.create_package_sandbox(&self.info.id, args)
    }

    /// The sandboxes made of it that the client's credentials made.
    pub fn list_sandbox(&self) -> Call<Vec<Sandbox>> {
        self.client.list_package_sandbox(&self.info.id)
    }

    /// A new grant to the sandbox `sandbox`, one [`list_sandbox`](Self::list_sandbox) finds.
    pub fn grant_sandbox(&self, sandbox: impl AsRef<str>) -> Call<PackageSandbox> {
        self.client.grant_package_sandbox(&self.info.id, sandbox)
    }

    /// Remove it. The sandboxes made of it stay, each with its grant.
    pub fn remove(&self) -> Call<()> {
        self.client.remove_package(&self.info.id)
    }
}

impl std::fmt::Debug for Package {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Package").field(&self.info).finish()
    }
}
