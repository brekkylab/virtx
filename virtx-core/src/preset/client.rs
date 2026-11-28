//! The client's end of `/presets/` and the sandboxes made of each.

use http_over_x::segment;

use super::{Preset, PresetPutArgs, PresetSandbox, PresetSandboxCreateArgs};
use crate::{Client, Error};

/// The path of the preset `preset` names: by its id, or its name in the project set.
fn preset(preset: &str) -> String {
    format!("/presets/{}", segment(preset))
}

/// Every preset in the project.
pub(crate) async fn list(client: &Client) -> Result<Vec<Preset>, Error> {
    Ok(client.wire().get("/presets/", client.headers()).await?)
}

/// Keep `args` as the preset `name`, made or replaced in whole.
pub(crate) async fn put(
    client: &Client,
    name: &str,
    args: &PresetPutArgs,
) -> Result<Preset, Error> {
    Ok(client
        .wire()
        .put(&preset(name), client.headers(), args)
        .await?)
}

/// The preset `name` names.
pub(crate) async fn get(client: &Client, name: &str) -> Result<Preset, Error> {
    Ok(client.wire().get(&preset(name), client.headers()).await?)
}

/// Remove the preset `name` names, leaving the sandboxes made of it.
pub(crate) async fn remove(client: &Client, name: &str) -> Result<(), Error> {
    Ok(client
        .wire()
        .delete(&preset(name), client.headers())
        .await?)
}

/// Make a sandbox of the preset `name` names, as `args` says.
pub(crate) async fn create_sandbox(
    client: &Client,
    name: &str,
    args: &PresetSandboxCreateArgs,
) -> Result<PresetSandbox, Error> {
    let path = format!("{}/sandboxes/", preset(name));
    Ok(client.wire().post(&path, client.headers(), args).await?)
}
