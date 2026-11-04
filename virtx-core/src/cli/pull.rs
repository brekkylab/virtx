//! `pull`: an image from a registry, tagged on the server as it is named.

use crate::Client;
use clap::Args;

use crate::cli::{job, name::with_tag, registry::RegistryArgs};

#[derive(Debug, PartialEq, Args)]
pub(crate) struct PullArgs {
    #[arg(value_name = "NAME[:TAG]")]
    pub(crate) image: String,

    /// Print only the ref and digest, not the pull's log.
    #[arg(short, long)]
    pub(crate) quiet: bool,

    #[command(flatten)]
    pub(crate) registry: RegistryArgs,
}

/// Pull an image and print the ref and the digest it pulled.
pub(crate) async fn pull(client: &Client, args: PullArgs) -> anyhow::Result<()> {
    let reference = with_tag(&args.image);
    let job = client.pull_image(&reference, args.registry.auth()?).await?;
    let digest = job::follow(client, &job, args.quiet).await?;
    println!("{reference} {digest}");
    Ok(())
}
