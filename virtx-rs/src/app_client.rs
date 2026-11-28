//! A client of one sandbox an app's user makes of a preset.

use virtx_core::Auth;

use crate::{Client, Error, Sandbox};

/// One sandbox, made of a preset for whom its access lets in and reached with its grant alone:
/// what an app's user runs processes in, reads and writes the files of, and forwards the ports
/// of, through [`sandbox`](Self::sandbox), and deletes with [`close`](Self::close), or as it
/// drops, blocking until it is. The grant reaches nothing else, so neither does this.
pub struct AppClient {
    /// Its sandbox, until it is deleted.
    sandbox: Option<Sandbox>,
}

impl AppClient {
    /// Make a sandbox of the preset `preset`, by its id, on the server at `url`, as `auth` lets
    /// in -- [`Auth::None`] where the preset is public, or a token of the issuer its access
    /// names -- and reach it with its grant from then on, over the same client. Refused as the server refuses making
    /// it: `401` or `403` for one the preset does not let in, and `429` past its limits.
    pub async fn connect(url: &str, preset: &str, auth: Auth) -> Result<AppClient, Error> {
        let client = Client::connect(url, auth, None)?;
        let made = client.create_preset_sandbox(preset, None).await?;
        let Some(token) = made.grant else {
            return Err(Error::Call(
                virtx_core::Source::Server,
                "the server gave the sandbox no grant to reach it with".into(),
            ));
        };
        // From here on, the grant alone: what made it reaches nothing more through this client.
        client.set_auth(Auth::Bearer { token })?;
        Ok(AppClient {
            sandbox: Some(made.sandbox),
        })
    }

    /// Its sandbox, and the client with its grant that reaches it.
    pub fn sandbox(&self) -> &Sandbox {
        self.sandbox
            .as_ref()
            .expect("an app client holds its sandbox until it drops")
    }

    /// Delete its sandbox, and with it its grant, and close the client, answering once both are
    /// done.
    pub async fn close(mut self) -> Result<(), Error> {
        let sandbox = self
            .sandbox
            .take()
            .expect("an app client holds its sandbox until it drops");
        let removed = sandbox.remove().await;
        let client = sandbox.client().clone();
        drop(sandbox);
        client.close().await?;
        removed
    }
}

/// Deletes its sandbox, as [`AppClient::close`] does, waited for.
impl Drop for AppClient {
    fn drop(&mut self) {
        if let Some(sandbox) = self.sandbox.take() {
            let _ = sandbox.remove().wait();
        }
    }
}

impl std::fmt::Debug for AppClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AppClient").field(&self.sandbox).finish()
    }
}
