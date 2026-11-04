//! The test server over stdio, for the bindings' tests: every endpoint of
//! [`virtx_core::testing`].
//!
//! ```sh
//! cargo build -p virtx-core --features testing --example test_server
//! ```

use hyper::service::Service as _;
use hyper_util::service::TowerToHyperService;
use virtx_core::{serve_stdio, testing::router};

#[tokio::main]
async fn main() {
    let service = TowerToHyperService::new(router());
    let handle = move |request| {
        let service = service.clone();
        async move { service.call(request).await.unwrap_or_else(|e| match e {}) }
    };
    serve_stdio(handle, tokio::io::stdin(), tokio::io::stdout())
        .await
        .expect("serving stdio");
}
