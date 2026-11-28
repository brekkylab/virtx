//! A client of one sandbox an app's user makes of a package.

use serde_json::json;
use virtx_core::{Auth, PortSpec};

use crate::library::Opaque;
use crate::{Client, Error, Sandbox};

/// One sandbox, made of a package for whom its access lets in and reached with its grant alone:
/// what an app's user runs processes in, reads and writes the files of, and forwards the ports
/// of, through [`sandbox`](Self::sandbox). The grant reaches nothing else, so neither does this.
///
/// The sandbox lives until it is deleted, with [`remove`](Self::remove): closed or dropped, this
/// leaves it, to be come back to with [`resume`](Self::resume) and its [`grant`](Self::grant),
/// or with [`resume_as`](Self::resume_as) as the one who made it.
pub struct AppClient {
    sandbox: Sandbox,
    grant: String,
}

impl AppClient {
    /// Make a sandbox of the package `package`, by its id, on the server at `url`, as `auth`
    /// lets in -- [`Auth::None`] where the package is public, or a token of the issuer its
    /// access names -- with `ports` on local, this machine's, beside those the package gives it
    /// in the sandbox, each [`PortSide::Local`](crate::PortSide::Local); and reach it with its
    /// grant from then on, over the same client. Refused as the server refuses making it: `401`
    /// or `403` for one the package does not let in, `429` past its limits, and `400` for a
    /// port in the sandbox.
    pub async fn connect(
        url: &str,
        package: &str,
        auth: Auth,
        ports: impl IntoIterator<Item = PortSpec>,
    ) -> Result<AppClient, Error> {
        let ports: Vec<_> = ports.into_iter().collect();
        let args = json!({ "package": package, "ports": ports });
        let client = Client::connect(url, auth, None)?;
        let request = client.api.virtx_client_app_create;
        AppClient::reach(client, request, args).await
    }

    /// Reach again the sandbox `grant` reaches, on the server at `url`: refused `401` once the
    /// sandbox is deleted.
    pub async fn resume(url: &str, grant: &str) -> Result<AppClient, Error> {
        let args = json!({ "grant": grant });
        let client = Client::connect(url, Auth::None, None)?;
        let request = client.api.virtx_client_app_resume;
        AppClient::reach(client, request, args).await
    }

    /// Reach again the sandbox `sandbox`, one made of the package `package` with `auth`, as
    /// [`Client::list_package_sandbox`] finds it, with a new grant: for one who kept no grant,
    /// let in by who they are. Refused `401` with [`Auth::None`], and `404` for a sandbox they
    /// did not make.
    pub async fn resume_as(
        url: &str,
        package: &str,
        auth: Auth,
        sandbox: &str,
    ) -> Result<AppClient, Error> {
        let args = json!({ "package": package, "sandbox": sandbox });
        let client = Client::connect(url, auth, None)?;
        let request = client.api.virtx_client_app_resume_as;
        AppClient::reach(client, request, args).await
    }

    /// `client`, made one of a sandbox's by the library's `request`, which answers the sandbox
    /// and its grant: closed where it fails.
    async fn reach(
        client: Client,
        request: unsafe extern "C" fn(*mut Opaque, *const u8, usize) -> *mut Opaque,
        args: serde_json::Value,
    ) -> Result<AppClient, Error> {
        let call: crate::Call<virtx_core::PackageSandbox> = client.call(request, args);
        match call.await {
            Ok(made) => Ok(AppClient {
                sandbox: Sandbox::new(client, made.sandbox),
                grant: made.grant.unwrap_or_default(),
            }),
            Err(failed) => {
                client.close().await?;
                Err(failed)
            }
        }
    }

    /// Its sandbox, and the client with its grant that reaches it.
    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    /// The token that reaches its sandbox, to keep and [`resume`](Self::resume) with: as a
    /// password to it, kept where the app's user's other secrets are.
    pub fn grant(&self) -> &str {
        &self.grant
    }

    /// Close the client, leaving its sandbox as it is.
    pub async fn close(self) -> Result<(), Error> {
        let client = self.sandbox.client().clone();
        drop(self.sandbox);
        client.close().await
    }

    /// Delete its sandbox, and with it every grant to it, and close the client, answering once
    /// both are done.
    pub async fn remove(self) -> Result<(), Error> {
        let removed = self.sandbox.remove().await;
        let client = self.sandbox.client().clone();
        drop(self.sandbox);
        client.close().await?;
        removed
    }
}

impl std::fmt::Debug for AppClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AppClient").field(&self.sandbox).finish()
    }
}
