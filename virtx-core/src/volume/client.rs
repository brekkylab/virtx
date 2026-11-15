//! The client's end of `/volumes/` and the mounts in each volume.

use super::{
    VolumeCreateArgs, VolumeListOutputItem, VolumeMountCreateArgs, VolumeMountListOutputItem,
};
use crate::{
    Error,
    transport::{TransportClient, http, resolved, segment, segments},
};

/// The path of the volume `name`.
fn volume(name: &str) -> String {
    format!("/volumes/{}", segment(name))
}

/// Every volume the server holds.
pub(crate) async fn list(client: &TransportClient) -> Result<Vec<VolumeListOutputItem>, Error> {
    http::get(client, "/volumes/").await
}

/// Keep a new, empty volume as `args` says.
pub(crate) async fn create(
    client: &TransportClient,
    args: &VolumeCreateArgs,
) -> Result<VolumeListOutputItem, Error> {
    http::post(client, "/volumes/", args).await
}

/// The volume `name`.
pub(crate) async fn get(
    client: &TransportClient,
    name: &str,
) -> Result<VolumeListOutputItem, Error> {
    http::get(client, &volume(name)).await
}

/// Delete the volume `name` and everything in it.
pub(crate) async fn remove(client: &TransportClient, name: &str) -> Result<(), Error> {
    http::delete(client, &volume(name)).await
}

/// The mounts in the volume `name`, without their credentials.
pub(crate) async fn list_mount(
    client: &TransportClient,
    name: &str,
) -> Result<Vec<VolumeMountListOutputItem>, Error> {
    http::get(client, &format!("{}/mounts/", volume(name))).await
}

/// Mount `args`'s source at `path` in the volume `name`, a directory the mount creates.
pub(crate) async fn create_mount(
    client: &TransportClient,
    name: &str,
    path: &str,
    args: &VolumeMountCreateArgs,
) -> Result<VolumeMountListOutputItem, Error> {
    let path = format!("{}/mounts/{}", volume(name), mount(path)?);
    http::put(client, &path, args).await
}

/// `path`, a mount's in a volume, [as it is sent](segments): refused unsent where it resolves to
/// the volume's root, which is no mount's, and would go to the list of them.
fn mount(path: &str) -> Result<String, Error> {
    match resolved(path).is_empty() {
        true => Err(Error::Path(format!(
            "{path:?} is the volume's root, which no mount is at"
        ))),
        false => Ok(segments(path)),
    }
}

/// Take away the mount at `path` in the volume `name`, leaving its source as it is.
pub(crate) async fn remove_mount(
    client: &TransportClient,
    name: &str,
    path: &str,
) -> Result<(), Error> {
    let path = format!("{}/mounts/{}", volume(name), mount(path)?);
    http::delete(client, &path).await
}
