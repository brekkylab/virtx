//! The client's end of `/snapshots/`.

use super::{SnapshotCreateArgs, SnapshotListOutputItem};
use crate::{
    Error,
    transport::{TransportClient, http, segment},
};

/// Every snapshot the server holds.
pub(crate) async fn list(client: &TransportClient) -> Result<Vec<SnapshotListOutputItem>, Error> {
    http::get(client, "/snapshots/").await
}

/// Keep a new, empty upper layer as `args` says.
pub(crate) async fn create(
    client: &TransportClient,
    args: &SnapshotCreateArgs,
) -> Result<SnapshotListOutputItem, Error> {
    http::post(client, "/snapshots/", args).await
}

/// The snapshot `name`.
pub(crate) async fn get(
    client: &TransportClient,
    name: &str,
) -> Result<SnapshotListOutputItem, Error> {
    http::get(client, &format!("/snapshots/{}", segment(name))).await
}

/// Delete the snapshot `name` and its upper layer.
pub(crate) async fn remove(client: &TransportClient, name: &str) -> Result<(), Error> {
    http::delete(client, &format!("/snapshots/{}", segment(name))).await
}
