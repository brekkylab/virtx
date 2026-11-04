//! `save`: an image into an OCI archive here.

use std::path::PathBuf;

use crate::Client;
use clap::Args;

use crate::cli::{
    name::{gather, lookup},
    transfer,
};

#[derive(Debug, PartialEq, Args)]
pub(crate) struct SaveArgs {
    /// The archive to write, replaced once all of it is written.
    #[arg(short, long, value_name = "FILE")]
    pub(crate) output: PathBuf,

    /// Say nothing of each blob written.
    #[arg(short, long)]
    pub(crate) quiet: bool,

    /// The image, by a ref, image ID or digest.
    #[arg(value_name = "IMAGE")]
    pub(crate) image: String,
}

/// Write an image to an OCI image layout in a tar archive, as `skopeo` writes for
/// `oci-archive:`, and print its digest.
pub(crate) async fn save(client: &Client, args: SaveArgs) -> anyhow::Result<()> {
    let images = gather(client.list_image().await?);
    let reference = lookup(&args.image, &images)?.reference();
    let download = client.download_image(reference, &args.output);
    let saved = transfer::follow(download, args.quiet, "nothing was written").await?;
    println!("{}", saved.digest);
    Ok(())
}
