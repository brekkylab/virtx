//! The test server: the files endpoints of `PROTOCOL.md` over in-memory filesystems, as an
//! axum router for the crate's own tests and for the `files_server` example the bindings'
//! tests spawn.
//!
//! It serves the volume `v` and the snapshot `s`, each a filesystem of its own, and the
//! snapshot `locked`, in use, which answers `423` to everything.

use std::{collections::HashMap, sync::Arc};

use axum::{
    Router,
    extract::State,
    response::{IntoResponse, Response},
};
use dav_server::{DavHandler, memfs::MemFs};
use http::StatusCode;

#[derive(Clone)]
struct Endpoints {
    /// Each root's WebDAV handler by its path under the base URL, such as `/volumes/v`.
    roots: Arc<HashMap<String, DavHandler>>,
}

/// The files endpoints, to mount `/rpc` beside.
pub fn files_router() -> Router {
    let roots = ["/volumes/v", "/snapshots/s"]
        .into_iter()
        .map(|prefix| {
            let dav = DavHandler::builder()
                .filesystem(MemFs::new())
                .strip_prefix(prefix)
                .build_handler();
            (prefix.to_owned(), dav)
        })
        .collect();
    let endpoints = Endpoints {
        roots: Arc::new(roots),
    };
    Router::new()
        .route("/{kind}/{name}", axum::routing::any(files))
        .route("/{kind}/{name}/", axum::routing::any(files))
        .route("/{kind}/{name}/{*path}", axum::routing::any(files))
        .with_state(endpoints)
}

/// The first two segments of `path`, such as `/volumes/v`.
fn prefix(path: &str) -> String {
    path.split('/').take(3).collect::<Vec<_>>().join("/")
}

/// Answers a request under `/{kind}/{name}/` from that root's filesystem.
async fn files(State(endpoints): State<Endpoints>, request: axum::extract::Request) -> Response {
    let prefix = prefix(request.uri().path());
    if prefix == "/snapshots/locked" {
        return StatusCode::LOCKED.into_response();
    }
    let Some(dav) = endpoints.roots.get(&prefix) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    dav.handle(request).await.map(axum::body::Body::new)
}
