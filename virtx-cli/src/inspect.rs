//! `inspect`: images as JSON.

use clap::Args;
use virtx_core::Client;

use crate::{
    command::report,
    name::{Image, gather, lookup},
};

#[derive(Debug, PartialEq, Args)]
pub(crate) struct InspectArgs {
    #[arg(required = true, value_name = "IMAGE")]
    pub(crate) images: Vec<String>,
}

/// Print the named images as JSON, and fail if one is not there.
pub(crate) async fn inspect(client: &Client, args: InspectArgs) -> anyhow::Result<()> {
    let images = gather(client.list_image().await?);
    // Every name is looked up, as `docker inspect` does: the found ones print, and one
    // missing fails the command.
    let mut failed = false;
    let mut found: Vec<Image> = Vec::new();
    for name in &args.images {
        match lookup(name, &images) {
            Ok(named) => found.push(named.image().familiar()),
            Err(error) => {
                failed = true;
                eprintln!("virtx: {}", report(&error));
            }
        }
    }
    println!("{}", serde_json::to_string_pretty(&found)?);
    anyhow::ensure!(!failed, "not every image was found");
    Ok(())
}
