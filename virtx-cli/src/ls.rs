//! `ls`: every image, one line per ref.

use std::fmt::Write as _;

use clap::Args;
use virtx_core::Client;

use crate::name::{Image, gather, image_id};

#[derive(Debug, PartialEq, Args)]
pub(crate) struct LsArgs {
    /// Show each image's digest beside its ID.
    #[arg(long)]
    pub(crate) digests: bool,

    /// Only show image IDs, one per image; with --digests, the digests.
    #[arg(short, long)]
    pub(crate) quiet: bool,
}

/// List every image, as a table or as bare IDs.
pub(crate) async fn ls(client: &Client, LsArgs { digests, quiet }: LsArgs) -> anyhow::Result<()> {
    let images = gather(client.list_image().await?);
    if quiet {
        for image in &images {
            let id = if digests {
                image.digest.as_str()
            } else {
                image_id(&image.digest)
            };
            println!("{id}");
        }
    } else {
        print!("{}", table(&images, digests));
    }
    Ok(())
}

/// One line per ref. The digest column is there on request, as the ID names the image already.
fn table(images: &[Image], digests: bool) -> String {
    let rows: Vec<(&str, &Image)> = images
        .iter()
        .flat_map(|image| image.refs.iter().map(move |r| (r.as_str(), image)))
        .collect();
    let width = rows
        .iter()
        .map(|(reference, _)| reference.len())
        .chain(std::iter::once("REF".len()))
        .max()
        .unwrap_or_default();

    let mut out = String::new();
    let heading = if digests {
        "IMAGE ID      DIGEST"
    } else {
        "IMAGE ID"
    };
    writeln!(out, "{:width$}  {heading}", "REF").unwrap();
    for (reference, image) in rows {
        let id = image_id(&image.digest);
        if digests {
            writeln!(out, "{reference:width$}  {id:12}  {}", image.digest).unwrap();
        } else {
            writeln!(out, "{reference:width$}  {id}").unwrap();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_lists_every_ref() {
        let images = [
            Image {
                digest: "sha256:aaaaaaaaaaaaaaaa".into(),
                refs: vec!["jq:latest".into(), "jq:1".into()],
            },
            Image {
                digest: "sha256:bb".into(),
                refs: vec!["alpine:3.20".into()],
            },
        ];
        assert_eq!(
            table(&images, false),
            "REF          IMAGE ID\n\
             jq:latest    aaaaaaaaaaaa\n\
             jq:1         aaaaaaaaaaaa\n\
             alpine:3.20  bb\n"
        );
        assert_eq!(
            table(&images, true),
            "REF          IMAGE ID      DIGEST\n\
             jq:latest    aaaaaaaaaaaa  sha256:aaaaaaaaaaaaaaaa\n\
             jq:1         aaaaaaaaaaaa  sha256:aaaaaaaaaaaaaaaa\n\
             alpine:3.20  bb            sha256:bb\n"
        );
        assert_eq!(table(&[], false), "REF  IMAGE ID\n");
    }
}
