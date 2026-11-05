//! `pull`: an image from a registry, tagged on the server as it is named.

use clap::Args;

use crate::Client;
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

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::cli::{
        job::tests::{client, tags},
        registry::RegistryArgs,
    };

    #[tokio::test]
    async fn a_name_with_no_tag_is_pulled_as_latest() {
        let client = client().await;
        let args = PullArgs {
            image: "alpine".into(),
            quiet: true,
            registry: RegistryArgs::default(),
        };
        pull(&client, args).await.unwrap();
        assert!(tags(&client).await.contains_key("alpine:latest"));
    }
}
