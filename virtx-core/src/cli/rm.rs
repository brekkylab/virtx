//! `rm`: a ref untagged, or an image removed with every ref on it, or from one repository.

use clap::Args;

use crate::Client;
use crate::cli::{
    command::report,
    name::{Image, Named, familiar_ref, gather, image_id, lookup, repository},
    prompt::{confirm, emphasis},
};

#[derive(Debug, PartialEq, Args)]
pub(crate) struct RmArgs {
    #[arg(required = true, value_name = "IMAGE")]
    pub(crate) images: Vec<String>,

    /// Remove an image named by its ID or digest with several refs without asking.
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
/// with its last ref; an image named by its ID goes with every ref on it, and one named by
/// `name@digest` with every ref in that repository, after asking when that is several.
pub(crate) async fn rm(client: &Client, args: RmArgs) -> anyhow::Result<()> {
    let mut images = gather(client.list_image().await?);
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
        let untagged = untagged(&named);
        let asked = match named {
            Named::Ref { .. } => None,
            Named::Image(image) => Some((
                image_id(&image.digest).to_string(),
                "remove the image and every ref?".to_string(),
            )),
            Named::Digest { image, repository } => {
                let repository = familiar_ref(repository);
                Some((
                    format!("{repository}@{}", image_id(&image.digest)),
                    format!("remove the image from {repository}, with these refs?"),
                ))
            }
        };
        if let Some((subject, question)) = asked
            && untagged.len() > 1
            && !args.force
        {
            eprintln!("{} has {} refs:", emphasis(&subject, true), untagged.len());
            for reference in &untagged {
                eprintln!("  {}", familiar_ref(reference));
            }
            if !confirm(&question)? {
                continue;
            }
        }
        match remove(client, &plan(&named)).await {
            Ok(()) => {
                let deleted = untagged.len() == image.refs.len();
                for reference in untagged {
                    println!("Untagged: {}", familiar_ref(reference));
                }
                if deleted {
                    println!("Deleted: {}", image_id(&image.digest));
                }
            }
            Err(error) => {
                failed = true;
                eprintln!("virtx: {name}: {}", report(&error));
                continue;
            }
        }
        // The next name is looked up among what is left, so its last ref is seen as last.
        images = gather(client.list_image().await?);
    }
    anyhow::ensure!(!failed, "not every image was removed");
    Ok(())
}

/// The refs removing `named` takes off its image.
fn untagged<'a>(named: &Named<'a>) -> Vec<&'a str> {
    match *named {
        Named::Ref { reference, .. } => vec![reference],
        Named::Image(image) => image.refs.iter().map(String::as_str).collect(),
        Named::Digest {
            image,
            repository: repo,
        } => image
            .refs
            .iter()
            .map(String::as_str)
            .filter(|reference| repository(reference) == repo)
            .collect(),
    }
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
        Named::Digest { image, repository } => vec![by_digest(image, repository)],
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

    #[test]
    fn a_digest_takes_the_image_from_its_repository_alone() {
        let shared = image(&["jq:1", "jq:latest", "mine:1"]);
        let named = Named::Digest {
            image: &shared,
            repository: "jq",
        };
        assert_eq!(plan(&named), [Removal::Remove("jq@sha256:aa".into())]);
        assert_eq!(untagged(&named), ["jq:1", "jq:latest"]);
    }
}
