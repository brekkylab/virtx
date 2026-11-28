//! A client of one sandbox an app's user makes of a package.

use serde_json::json;
use virtx_core::{Auth, PortSpec};

use crate::library::Opaque;
use crate::{Client, Error, Sandbox};

/// One sandbox, made of a package for whom its access lets in, and reached as they are: what an
/// app's user runs processes in, reads and writes the files of, and forwards the ports of,
/// through [`sandbox`](Self::sandbox). Every request carries the token it was made with, which
/// reaches the sandboxes that user made of the package and nothing else.
///
/// The sandbox lives until it is deleted, with [`remove`](Self::remove): closed or dropped, this
/// leaves it, to be come back to with [`open`](Self::open) by its id.
pub struct AppClient {
    sandbox: Sandbox,
}

impl AppClient {
    /// Make a sandbox of the package named `package` in the project `project`, by the project's
    /// id, on the server at `url`, as `auth` lets in -- a token of the issuer the package's access names,
    /// sent with every request from then on -- with `ports` on local, this machine's, beside those
    /// the package gives it in the sandbox, each [`PortSide::Local`](crate::PortSide::Local).
    /// Refused as the server refuses making it: `401` or `403` for one the package does not let
    /// in, `429` past its limits, and `400` for a port in the sandbox.
    pub async fn create(
        url: &str,
        project: &str,
        package: &str,
        auth: Auth,
        ports: impl IntoIterator<Item = PortSpec>,
    ) -> Result<AppClient, Error> {
        let ports: Vec<_> = ports.into_iter().collect();
        let args = json!({ "package": package, "ports": ports });
        let client = Client::connect(url, auth, Some(project))?;
        let request = client.api.virtx_client_create_package_sandbox;
        AppClient::reach(client, request, args).await
    }

    /// Reach again the sandbox `sandbox` in the project `project`, each by its id, one made of a
    /// package as `auth` lets in, such as [`Client::list_package_sandbox`] finds: refused `401`
    /// or `403` for one they did not make, or one deleted.
    pub async fn open(
        url: &str,
        project: &str,
        auth: Auth,
        sandbox: &str,
    ) -> Result<AppClient, Error> {
        let args = json!({ "id": sandbox });
        let client = Client::connect(url, auth, Some(project))?;
        let request = client.api.virtx_client_get_sandbox;
        AppClient::reach(client, request, args).await
    }

    /// `client`, made one of a sandbox's by the library's `request`, which answers the sandbox:
    /// closed where it fails.
    async fn reach(
        client: Client,
        request: unsafe extern "C" fn(*mut Opaque, *const u8, usize) -> *mut Opaque,
        args: serde_json::Value,
    ) -> Result<AppClient, Error> {
        let call: crate::Call<virtx_core::Sandbox> = client.call(request, args);
        match call.await {
            Ok(info) => Ok(AppClient {
                sandbox: Sandbox::new(client, info),
            }),
            Err(failed) => {
                client.close().await?;
                Err(failed)
            }
        }
    }

    /// Its sandbox, and the client that reaches it.
    pub fn sandbox(&self) -> &Sandbox {
        &self.sandbox
    }

    /// Close the client, leaving its sandbox as it is.
    pub async fn close(self) -> Result<(), Error> {
        let client = self.sandbox.client().clone();
        drop(self.sandbox);
        client.close().await
    }

    /// Delete its sandbox, and close the client, answering once both are done.
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
