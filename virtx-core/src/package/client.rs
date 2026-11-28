//! The client's end of `/packages/` and the sandboxes made of each.

use http_over_x::segment;

use super::{Package, PackagePutArgs, PackageSandboxCreateArgs};
use crate::{Client, Error, Sandbox};

/// The path of the package `package` names: by its id, or its name in the project set.
fn package(package: &str) -> String {
    format!("/packages/{}", segment(package))
}

/// Every package in the project.
pub(crate) async fn list(client: &Client) -> Result<Vec<Package>, Error> {
    Ok(client.wire().get("/packages/", client.headers()).await?)
}

/// Keep `args` as the package `name`, made or replaced in whole.
pub(crate) async fn put(
    client: &Client,
    name: &str,
    args: &PackagePutArgs,
) -> Result<Package, Error> {
    Ok(client
        .wire()
        .put(&package(name), client.headers(), args)
        .await?)
}

/// The package `name` names.
pub(crate) async fn get(client: &Client, name: &str) -> Result<Package, Error> {
    Ok(client.wire().get(&package(name), client.headers()).await?)
}

/// Remove the package `name` names, leaving the sandboxes made of it.
pub(crate) async fn remove(client: &Client, name: &str) -> Result<(), Error> {
    Ok(client
        .wire()
        .delete(&package(name), client.headers())
        .await?)
}

/// Make a sandbox of the package `name` names, as `args` says.
pub(crate) async fn create_sandbox(
    client: &Client,
    name: &str,
    args: &PackageSandboxCreateArgs,
) -> Result<Sandbox, Error> {
    let path = format!("{}/sandboxes/", package(name));
    Ok(client.wire().post(&path, client.headers(), args).await?)
}

/// The sandboxes made of the package `name` names that the client made.
pub(crate) async fn list_sandbox(client: &Client, name: &str) -> Result<Vec<Sandbox>, Error> {
    let path = format!("{}/sandboxes/", package(name));
    Ok(client.wire().get(&path, client.headers()).await?)
}
