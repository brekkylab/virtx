//! `tag`: a ref pointed at an image.

use clap::Args;

use crate::Client;
use crate::cli::name::{gather, lookup, with_tag};

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

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::cli::{
        job::tests::{client, tags},
        name::image_id,
    };

    fn args(source: &str, target: &str) -> TagArgs {
        TagArgs {
            source: source.into(),
            target: target.into(),
        }
    }

    #[tokio::test]
    async fn a_tag_points_at_the_image_its_source_names() {
        let client = client().await;
        let alpine = tags(&client).await["alpine:3.20"].clone();
        tag(&client, args("alpine:3.20", "mine")).await.unwrap();
        tag(&client, args(image_id(&alpine), "mine:by-id"))
            .await
            .unwrap();
        let tags = tags(&client).await;
        assert_eq!(tags.get("mine:latest"), Some(&alpine));
        assert_eq!(tags.get("mine:by-id"), Some(&alpine));
        assert!(tag(&client, args("missing:1", "mine:2")).await.is_err());
    }
}
