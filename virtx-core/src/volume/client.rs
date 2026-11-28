//! The client's end of `/volumes/` and the mounts in each volume.

use super::{
    VolumeCreateArgs, VolumeListOutputItem, VolumeMountCreateArgs, VolumeMountListOutputItem,
};
use crate::{Client, Error};
use http_over_x::{resolved, segment, segments};

/// The path of the volume `name`.
fn volume(name: &str) -> String {
    format!("/volumes/{}", segment(name))
}

/// Every volume the server holds.
pub(crate) async fn list(client: &Client) -> Result<Vec<VolumeListOutputItem>, Error> {
    Ok(client.wire().get("/volumes/", client.headers()).await?)
}

/// Keep a new, empty volume as `args` says.
pub(crate) async fn create(
    client: &Client,
    args: &VolumeCreateArgs,
) -> Result<VolumeListOutputItem, Error> {
    Ok(client
        .wire()
        .post("/volumes/", client.headers(), args)
        .await?)
}

/// The volume `name`.
pub(crate) async fn get(client: &Client, name: &str) -> Result<VolumeListOutputItem, Error> {
    Ok(client.wire().get(&volume(name), client.headers()).await?)
}

/// Delete the volume `name` and everything in it.
pub(crate) async fn remove(client: &Client, name: &str) -> Result<(), Error> {
    Ok(client
        .wire()
        .delete(&volume(name), client.headers())
        .await?)
}

/// The mounts in the volume `name`, without their credentials.
pub(crate) async fn list_mount(
    client: &Client,
    name: &str,
) -> Result<Vec<VolumeMountListOutputItem>, Error> {
    Ok(client
        .wire()
        .get(&format!("{}/mounts/", volume(name)), client.headers())
        .await?)
}

/// Mount `args`'s source at `path` in the volume `name`, a directory the mount creates.
pub(crate) async fn create_mount(
    client: &Client,
    name: &str,
    path: &str,
    args: &VolumeMountCreateArgs,
) -> Result<VolumeMountListOutputItem, Error> {
    let path = format!("{}/mounts/{}", volume(name), mount(path)?);
    Ok(client.wire().put(&path, client.headers(), args).await?)
}

/// `path`, a mount's in a volume, [as it is sent](segments): refused unsent where it resolves to
/// the volume's root, which is no mount's, and would go to the list of them.
fn mount(path: &str) -> Result<String, Error> {
    match resolved(path).is_empty() {
        true => Err(Error::Client(format!(
            "{path:?} is the volume's root, which no mount is at"
        ))),
        false => Ok(segments(path)),
    }
}

/// Take away the mount at `path` in the volume `name`, leaving its source as it is.
pub(crate) async fn remove_mount(client: &Client, name: &str, path: &str) -> Result<(), Error> {
    let path = format!("{}/mounts/{}", volume(name), mount(path)?);
    Ok(client.wire().delete(&path, client.headers()).await?)
}
