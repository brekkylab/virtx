//! The test server over stdio, for the bindings' tests: the files endpoint of
//! [`virtx_core::testing`], and `/rpc` with no method behind it.
//!
//! ```sh
//! cargo build -p virtx-core --features testing --example files_server
//! ```

use hyper::service::Service as _;
use hyper_util::service::TowerToHyperService;
use virtx_core::{Dispatcher, Methods, Server, serve_stdio, testing::files_router};

#[tokio::main]
async fn main() {
    let rpc = Server::new(Dispatcher::new(Methods::new()));
    let router = files_router().route(
        "/rpc",
        axum::routing::any(move |request: axum::extract::Request| {
            let rpc = rpc.clone();
            async move { rpc.handle(request).await }
        }),
    );
    let service = TowerToHyperService::new(router);
    let handle = move |request| {
        let service = service.clone();
        async move { service.call(request).await.unwrap_or_else(|e| match e {}) }
    };
    serve_stdio(handle, tokio::io::stdin(), tokio::io::stdout())
        .await
        .expect("serving stdio");
}
