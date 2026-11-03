//! `rm`: a ref untagged, or an image removed with every ref on it.

use clap::Args;
use virtx_core::Client;

use crate::{
    command::report,
    name::{Image, Named, gather, image_id, lookup, repository},
    prompt::{confirm, emphasis},
};

#[derive(Debug, PartialEq, Args)]
pub(crate) struct RmArgs {
    #[arg(required = true, value_name = "IMAGE")]
    pub(crate) images: Vec<String>,

    /// Remove an image named by its ID with several refs without asking.
    #[arg(short, long)]
    pub(crate) force: bool,
}

/// What the server is asked to do for one name.
#[derive(Debug, PartialEq, Eq)]
enum Removal {
    /// Take the tag off; the image stays under its other tags in that repository.
    Untag(String),
    /// Remove the image from a repository, with every tag there on it.
    Remove(String),
}

/// Remove what each name names, as `docker rmi` does: a ref is untagged, and the image goes
/// with its last ref; an image named by its ID goes with every ref on it, after asking when it
/// has several.
pub(crate) async fn rm(client: &Client, args: RmArgs) -> anyhow::Result<()> {
    let images = gather(client.list_image().await?);
    // Every name is tried, and one failure fails the command at the end rather than leaving
    // the rest. A declined one is not a failure.
    let mut failed = false;
    for name in &args.images {
        let named = match lookup(name, &images) {
            Ok(named) => named,
            Err(error) => {
                failed = true;
                eprintln!("virtx: {}", report(&error));
                continue;
            }
        };
        let image = named.image();
        if let Named::Image(_) = named
            && image.refs.len() > 1
            && !args.force
        {
            eprintln!(
                "{} has {} refs:",
                emphasis(image_id(&image.digest), true),
                image.refs.len()
            );
            for reference in &image.refs {
                eprintln!("  {reference}");
            }
            if !confirm("remove the image and every ref?")? {
                continue;
            }
        }
        match remove(client, &plan(&named)).await {
            Ok(()) => {
                let (untagged, deleted) = match named {
                    Named::Ref { reference, .. } => (vec![reference], image.refs.len() == 1),
                    Named::Image(_) => (image.refs.iter().map(String::as_str).collect(), true),
                };
                for reference in untagged {
                    println!("Untagged: {reference}");
                }
                if deleted {
                    println!("Deleted: {}", image_id(&image.digest));
                }
            }
            Err(error) => {
                failed = true;
                eprintln!("virtx: {name}: {}", report(&error));
            }
        }
    }
    anyhow::ensure!(!failed, "not every image was removed");
    Ok(())
}

async fn remove(client: &Client, plan: &[Removal]) -> anyhow::Result<()> {
    for removal in plan {
        match removal {
            Removal::Untag(reference) => client.untag_image(reference).await?,
            Removal::Remove(reference) => client.remove_image(reference).await?,
        }
    }
    Ok(())
}

/// What removing `named` takes. The server holds an image once per repository, so a ref is
/// untagged while another tag in its repository holds the image, and the image is removed
/// from the repository with its last one there.
fn plan(named: &Named) -> Vec<Removal> {
    let by_digest =
        |image: &Image, repository: &str| Removal::Remove(format!("{repository}@{}", image.digest));
    match *named {
        Named::Ref { image, reference } => {
            let repo = repository(reference);
            let shared = image
                .refs
                .iter()
                .any(|other| other != reference && repository(other) == repo);
            if shared {
                vec![Removal::Untag(reference.to_string())]
            } else {
                vec![by_digest(image, repo)]
            }
        }
        Named::Image(image) => {
            let mut repositories: Vec<&str> = Vec::new();
            for repo in image.refs.iter().map(|r| repository(r)) {
                if !repositories.contains(&repo) {
                    repositories.push(repo);
                }
            }
            repositories
                .into_iter()
                .map(|repo| by_digest(image, repo))
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(refs: &[&str]) -> Image {
        Image {
            digest: "sha256:aa".into(),
            refs: refs.iter().map(|r| r.to_string()).collect(),
        }
    }

    #[test]
    fn a_ref_is_untagged_while_its_repository_holds_the_image_under_another() {
        let shared = image(&["jq:1", "jq:latest", "mine:1"]);
        assert_eq!(
            plan(&Named::Ref {
                image: &shared,
                reference: "jq:1"
            }),
            [Removal::Untag("jq:1".into())]
        );
        assert_eq!(
            plan(&Named::Ref {
                image: &shared,
                reference: "mine:1"
            }),
            [Removal::Remove("mine@sha256:aa".into())]
        );
    }

    #[test]
    fn an_image_is_removed_from_every_repository_it_is_in() {
        let shared = image(&["jq:1", "jq:latest", "mine:1"]);
        assert_eq!(
            plan(&Named::Image(&shared)),
            [
                Removal::Remove("jq@sha256:aa".into()),
                Removal::Remove("mine@sha256:aa".into()),
            ]
        );
    }
}
