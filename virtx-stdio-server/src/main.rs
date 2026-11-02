//! `virtx-uvm` -- a virtx server over stdio: the RPC `virtx-core` names at `/rpc`, and the
//! images as an OCI registry at `/images/v2/` (see [`virtx_uvm::RegistryServer`]) and the
//! files of its snapshots and volumes at `/volumes/` and `/snapshots/`, answered on stdin
//! and stdout to the client that spawned it, until it closes stdin. Everything this host
//! keeps is that one client's.
//!
//! `virtx-uvm vm …` is this binary spawned by itself to be a machine; see
//! [`virtx_uvm::Machine`].

mod rpc;

use std::{io, sync::Arc};

use http_body_util::BodyExt as _;
use virtx_core::{Dispatcher, Server, serve_stdio};
use virtx_uvm::{ImageStore, RegistryServer};

use crate::rpc::Home;

fn main() -> anyhow::Result<()> {
    // Before any runtime: this process is about to become a machine and never come back.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == virtx_uvm::MACHINE_SUBCOMMAND)
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
    let rpc = Server::new(Dispatcher::new(rpc::methods(&home)));
    let registry = Arc::new(RegistryServer::new(images, "/images")?);
    let handle = move |request: http::Request<_>| {
        let (rpc, registry, home) = (rpc.clone(), Arc::clone(&registry), Arc::clone(&home));
        async move {
            let path = request.uri().path();
            if path == "/rpc" {
                rpc.handle(request)
                    .await
                    .map(|body| body.map_err(|never| match never {}).boxed_unsync())
            } else if path == "/images/v2" || path.starts_with("/images/v2/") {
                registry.handle(request).await
            } else {
                let request = request.map(axum::body::Body::new);
                home.files
                    .handle(request, &|name| home.holds_snapshot(name))
                    .await
                    .map(|body| body.map_err(io::Error::other).boxed_unsync())
            }
        }
    };
    // The client is gone, and every sandbox was its: a command still running ends with this
    // process, whose machines are killed as their calls drop.
    Ok(serve_stdio(handle, tokio::io::stdin(), tokio::io::stdout()).await?)
}
