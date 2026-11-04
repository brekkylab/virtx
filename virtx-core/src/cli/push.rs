//! `push`: an image to the registry its ref names.

use clap::Args;

use crate::Client;
use crate::cli::{job, name::with_tag, registry::RegistryArgs};

#[derive(Debug, PartialEq, Args)]
pub(crate) struct PushArgs {
    /// The ref to push, which also names where it goes, as `ghcr.io/owner/name:tag`.
    #[arg(value_name = "NAME[:TAG]")]
    pub(crate) image: String,

    /// Print only the ref and digest, not the push's log.
    #[arg(short, long)]
    pub(crate) quiet: bool,

    #[command(flatten)]
    pub(crate) registry: RegistryArgs,
}

/// Push an image and print the ref and the digest it pushed.
pub(crate) async fn push(client: &Client, args: PushArgs) -> anyhow::Result<()> {
    let reference = with_tag(&args.image);
    let job = client.push_image(&reference, args.registry.auth()?).await?;
    let digest = job::follow(client, &job, args.quiet).await?;
    println!("{reference} {digest}");
    Ok(())
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_push_ends_with_the_digest_it_pushed() {
        let client = job::tests::client().await;
        let job = client.push_image("jq:latest", None).await.unwrap();
        assert_eq!(job::follow(&client, &job, true).await.unwrap(), "sha256:0");
    }
}
