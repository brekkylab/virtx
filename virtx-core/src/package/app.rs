//! What an app's user reaches the one sandbox they make of a package with: its grant alone.

use std::sync::Arc;

use super::{PackageSandbox, PackageSandboxCreateArgs};
use crate::{Auth, Client, Error, PortSpec, Sandbox};

/// One sandbox, made of a package for whom its access lets in and reached with its grant alone,
/// over a client that sends that grant and nothing else from the moment this is made. The
/// sandbox lives until it is deleted: this is come back to with [`resume`](Self::resume) and the
/// grant, or with [`resume_as`](Self::resume_as) as the one who made it.
pub struct AppClient {
    client: Arc<Client>,
    sandbox: Sandbox,
    grant: String,
}

impl AppClient {
    /// Make a sandbox of the package `package`, by its id, through `client`, connected with
    /// what the package's access lets in, with `ports` on local beside those the package gives
    /// it in the sandbox; and reach it with its grant through `client` from then on.
    pub async fn create(
        client: Arc<Client>,
        package: &str,
        ports: Vec<PortSpec>,
    ) -> Result<AppClient, Error> {
        let args = PackageSandboxCreateArgs { name: None, ports };
        let made = client.create_package_sandbox(package, &args).await?;
        AppClient::reaching(client, made).await
    }

    /// Reach again, through `client`, the sandbox `grant` reaches: refused `401` once the
    /// sandbox is deleted.
    pub async fn resume(client: Arc<Client>, grant: String) -> Result<AppClient, Error> {
        client.set_auth(Auth::Bearer {
            token: grant.clone(),
        })?;
        // A grant's listing is its own sandbox alone.
        let Some(sandbox) = client.list_sandbox().await?.pop() else {
            return Err(Error::Client(
                "the server answered no sandbox to the grant".into(),
            ));
        };
        Ok(AppClient {
            client,
            sandbox,
            grant,
        })
    }

    /// Reach again the sandbox `sandbox`, one made of the package `package` with what `client`
    /// is connected with, as [`Client::list_package_sandbox`] finds it, with a new grant: for
    /// one who kept no grant, let in by who they are. Refused `401` with no credentials, and
    /// `404` for a sandbox they did not make.
    pub async fn resume_as(
        client: Arc<Client>,
        package: &str,
        sandbox: &str,
    ) -> Result<AppClient, Error> {
        let made = client.grant_package_sandbox(package, sandbox).await?;
        AppClient::reaching(client, made).await
    }

    /// `made`, reached through `client` with its grant alone from now on.
    async fn reaching(client: Arc<Client>, made: PackageSandbox) -> Result<AppClient, Error> {
        let Some(grant) = made.grant else {
            // Nothing would reach it with no grant: it goes.
            client.remove_sandbox(&made.sandbox.id).await?;
            return Err(Error::Client(
                "the server gave the sandbox no grant to reach it with".into(),
            ));
        };
        // From here on, the grant alone: what made it reaches nothing more through this client.
        client.set_auth(Auth::Bearer {
            token: grant.clone(),
        })?;
        Ok(AppClient {
            client,
            sandbox: made.sandbox,
            grant,
        })
    }

    /// The client, sending the grant alone.
    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }

    /// Its sandbox, as the server answered it.
    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    /// The token that reaches its sandbox, to keep and [`resume`](Self::resume) with.
    pub fn grant(&self) -> &str {
        &self.grant
    }
}

impl From<AppClient> for PackageSandbox {
    fn from(app: AppClient) -> PackageSandbox {
        PackageSandbox {
            sandbox: app.sandbox,
            grant: Some(app.grant),
        }
    }
}

impl std::fmt::Debug for AppClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AppClient").field(&self.sandbox).finish()
    }
}
