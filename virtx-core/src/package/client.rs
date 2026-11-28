//! The client's end of `/packages/` and the sandboxes made of each.

use http_over_x::segment;

use super::{Package, PackagePutArgs, PackageSandboxCreateArgs};
use crate::{Client, Error, PortSpec, Sandbox};

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

/// Make a sandbox of the package `id` for an app's user, with `ports` on local, and name the
/// package's project through `client` from then on, which the sandbox is reached in.
pub(crate) async fn app_create(
    client: &Client,
    id: &str,
    ports: Vec<PortSpec>,
) -> Result<Sandbox, Error> {
    client.set_project(Some(project_of(id)?))?;
    let args = PackageSandboxCreateArgs { name: None, ports };
    create_sandbox(client, id, &args).await
}

/// The sandbox `sandbox`, one the app's user made of the package `id`, its project named through
/// `client` from now on.
pub(crate) async fn app_resume(client: &Client, id: &str, sandbox: &str) -> Result<Sandbox, Error> {
    client.set_project(Some(project_of(id)?))?;
    client.get_sandbox(sandbox).await
}

/// The id of the project the package `id` is in, which the package's id starts with.
fn project_of(id: &str) -> Result<&str, Error> {
    match id.split_once('.') {
        Some((project, _)) if !project.is_empty() => Ok(project),
        _ => Err(Error::Client(format!(
            "{id:?} is no package's id: an app names its package by the id put_package answered"
        ))),
    }
}
