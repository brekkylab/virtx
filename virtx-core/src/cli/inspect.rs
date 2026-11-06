//! `inspect`: images as JSON.

use clap::Args;

use crate::Client;
use crate::cli::{
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

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::cli::job::tests::client;

    fn args(images: &[&str]) -> InspectArgs {
        InspectArgs {
            images: images.iter().map(|image| image.to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn every_name_is_looked_up_and_one_not_there_fails_the_command() {
        let client = client().await;
        inspect(&client, args(&["alpine:3.20"])).await.unwrap();
        let missing = inspect(&client, args(&["alpine:3.20", "missing:1"]))
            .await
            .unwrap_err();
        assert_eq!(missing.to_string(), "not every image was found");
    }
}
