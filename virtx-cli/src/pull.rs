//! `pull`: an image from a registry, tagged on the server as it is named.

use clap::Args;
use virtx_core::Client;

use crate::{job, name::with_tag};

#[derive(Debug, PartialEq, Args)]
pub(crate) struct PullArgs {
    #[arg(value_name = "NAME[:TAG]")]
    pub(crate) image: String,

    /// Print only the ref and digest, not the pull's log.
    #[arg(short, long)]
    pub(crate) quiet: bool,
}

/// Pull an image and print the ref and the digest it pulled.
pub(crate) async fn pull(client: &Client, args: PullArgs) -> anyhow::Result<()> {
    let reference = with_tag(&args.image);
    let job = client.pull_image(&reference, None).await?;
    let digest = job::follow(client, &job, args.quiet).await?;
    println!("{reference} {digest}");
    Ok(())
}
