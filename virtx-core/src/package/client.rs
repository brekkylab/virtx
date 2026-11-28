//! The client's end of `/packages/` and the sandboxes made of each.

use http_over_x::segment;

use super::{AppSandbox, Package, PackagePutArgs, PackageSandbox, PackageSandboxCreateArgs};
use crate::{Auth, Client, Error, PortSpec, Sandbox};

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
) -> Result<PackageSandbox, Error> {
    let path = format!("{}/sandboxes/", package(name));
    Ok(client.wire().post(&path, client.headers(), args).await?)
}

/// The sandboxes made of the package `name` names that the client made.
pub(crate) async fn list_sandbox(client: &Client, name: &str) -> Result<Vec<Sandbox>, Error> {
    let path = format!("{}/sandboxes/", package(name));
    Ok(client.wire().get(&path, client.headers()).await?)
}

/// A new grant to the sandbox `id`, one the client made of the package `name` names.
pub(crate) async fn grant_sandbox(
    client: &Client,
    name: &str,
    id: &str,
) -> Result<PackageSandbox, Error> {
    let path = format!("{}/sandboxes/{}/grant", package(name), segment(id));
    let empty = serde_json::json!({});
    Ok(client.wire().post(&path, client.headers(), &empty).await?)
}

/// Make a sandbox of the package `name` names for an app's user, with `ports` on local, and
/// send its grant alone through `client` from then on.
pub(crate) async fn app_create(
    client: &Client,
    name: &str,
    ports: Vec<PortSpec>,
) -> Result<AppSandbox, Error> {
    let args = PackageSandboxCreateArgs { name: None, ports };
    let made = create_sandbox(client, name, &args).await?;
    app_reached(client, made).await
}

/// Send `grant` alone through `client` from now on, and answer the sandbox it reaches.
pub(crate) async fn app_resume(client: &Client, grant: String) -> Result<AppSandbox, Error> {
    client.set_auth(Auth::Bearer {
        token: grant.clone(),
    })?;
    // A grant's listing is its own sandbox alone.
    let Some(sandbox) = client.list_sandbox().await?.pop() else {
        return Err(Error::Client(
            "the server answered no sandbox to the grant".into(),
        ));
    };
    Ok(AppSandbox { sandbox, grant })
}

/// A new grant to the sandbox `id`, one the client made of the package `name` names, sent alone
/// through `client` from then on.
pub(crate) async fn app_resume_as(
    client: &Client,
    name: &str,
    id: &str,
) -> Result<AppSandbox, Error> {
    let made = grant_sandbox(client, name, id).await?;
    app_reached(client, made).await
}

/// `made`, reached through `client` with its grant alone from now on.
async fn app_reached(client: &Client, made: PackageSandbox) -> Result<AppSandbox, Error> {
    let Some(grant) = made.grant else {
        // Nothing would reach it with no grant: it goes.
        client.remove_sandbox(&made.sandbox.id).await?;
        return Err(Error::Client(
            "the server gave the sandbox no grant to reach it with".into(),
        ));
    };
    // From here on, the grant alone: what made it reaches nothing more through this client.
    client.set_auth(Auth::Bearer {
        token: grant.clone(),
    })?;
    Ok(AppSandbox {
        sandbox: made.sandbox,
        grant,
    })
}
