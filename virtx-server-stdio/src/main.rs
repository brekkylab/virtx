//! `virtx-stdio` -- a virtx server over stdio: the endpoints `virtx-core`'s `PROTOCOL.md` names,
//! with the images as an OCI registry at `/images/oci/v2/` (see [`virtx_uvm::RegistryServer`]),
//! answered on stdin and stdout to the client that spawned it, until it closes stdin. Everything
//! this host keeps is that one client's.
//!
//! `virtx-stdio vm …` is this binary spawned by itself to be a machine; see
//! [`virtx_uvm::Machine`].

mod endpoints;

use std::sync::Arc;

use axum::Extension;
use tower::ServiceExt as _;
use virtx_core::serve_stdio;
use virtx_uvm::{ImageStore, RegistryServer};

use crate::endpoints::Home;

/// What this binary is spawned with to be a machine.
const MACHINE_SUBCOMMAND: &str = "vm";

fn main() -> anyhow::Result<()> {
    // Before any runtime: this process is about to become a machine and never come back.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == MACHINE_SUBCOMMAND)
    {
        // Why it could not be is the host's already, told on the control channel; stderr is
        // the guest command's, and nothing of this process's goes there.
        let Err(_) = virtx_uvm::enter_machine();
        std::process::exit(1);
    }
    serve()
}

#[tokio::main]
async fn serve() -> anyhow::Result<()> {
    let root = virtx_uvm::cache_root();
    let images = ImageStore::new(root.join("images"))?;
    let home = Arc::new(Home::new(images.clone(), &root)?);
    let api = endpoints::router().layer(Extension(Arc::clone(&home)));
    let registry = Arc::new(RegistryServer::new(images, "/images/oci")?);
    let handle = move |request: http::Request<_>| {
        let (api, registry) = (api.clone(), Arc::clone(&registry));
        async move {
            let path = request.uri().path();
            if path == "/images/oci/v2" || path.starts_with("/images/oci/v2/") {
                registry.handle(request).await.map(axum::body::Body::new)
            } else {
                let Ok(response) = api.oneshot(request).await;
                response
            }
        }
    };
    let served = serve_stdio(handle, tokio::io::stdin(), tokio::io::stdout()).await;
    // The client is gone, and every sandbox was its.
    tokio::task::spawn_blocking(move || home.close_all()).await?;
    Ok(served?)
}
