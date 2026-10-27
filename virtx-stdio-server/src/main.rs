//! `virtx-uvm` -- a virtx server over stdio: the RPC `virtx-core` names, answered on stdin and
//! stdout to the client that spawned it, until it closes stdin. Everything this host keeps is
//! that one client's.
//!
//! `virtx-uvm vm …` is this binary spawned by itself to be a machine; see
//! [`virtx_uvm::Machine`].

mod rpc;

use std::sync::Arc;

use virtx_core::{Dispatcher, serve_stdio};

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
    let home = Arc::new(Home::new(&virtx_uvm::cache_root())?);
    let dispatcher = Dispatcher::new(rpc::methods(&home));
    // The client is gone, and every sandbox was its: a command still running ends with this
    // process, whose machines are killed as their calls drop.
    Ok(serve_stdio(dispatcher, tokio::io::stdin(), tokio::io::stdout()).await?)
}
