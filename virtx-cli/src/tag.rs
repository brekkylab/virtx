//! `tag`: a ref pointed at an image.

use clap::Args;
use virtx_core::Client;

use crate::name::{gather, lookup, with_tag};

#[derive(Debug, PartialEq, Args)]
pub(crate) struct TagArgs {
    /// The image to tag, by a ref, image ID or digest.
    #[arg(value_name = "SOURCE")]
    pub(crate) source: String,

    /// The ref to point at it, moved there if it is in use.
    #[arg(value_name = "TARGET[:TAG]")]
    pub(crate) target: String,
}

/// Point `target` at the image `source` names.
pub(crate) async fn tag(client: &Client, args: TagArgs) -> anyhow::Result<()> {
    let images = gather(client.list_image().await?);
    let source = lookup(&args.source, &images)?.reference();
    client.tag_image(source, with_tag(&args.target)).await?;
    Ok(())
}
