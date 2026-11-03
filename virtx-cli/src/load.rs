//! `load`: an image from an OCI archive here.

use std::path::PathBuf;

use clap::Args;
use virtx_core::Client;

use crate::{name::with_tag, transfer};

#[derive(Debug, PartialEq, Args)]
pub(crate) struct LoadArgs {
    /// The archive to read: an OCI image layout in a tar archive.
    #[arg(short, long, value_name = "FILE")]
    pub(crate) input: PathBuf,

    /// The ref to tag the image with; the archive's only image, or the one it tags so.
    #[arg(short, long, value_name = "NAME[:TAG]")]
    pub(crate) tag: String,

    /// Say nothing of each blob uploaded.
    #[arg(short, long)]
    pub(crate) quiet: bool,
}

/// Upload the image in an archive, skipping blobs the server has, and print the ref and the
/// digest it tags.
pub(crate) async fn load(client: &Client, args: LoadArgs) -> anyhow::Result<()> {
    let reference = with_tag(&args.tag);
    let upload = client.upload_image(&args.input, &reference);
    let loaded = transfer::follow(upload, args.quiet, "no tag was moved").await?;
    println!("{reference} {}", loaded.digest);
    Ok(())
}
