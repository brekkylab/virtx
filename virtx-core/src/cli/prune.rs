//! `prune`: every image no ref points at and no sandbox uses.

use clap::Args;

use crate::cli::prompt::confirm;
use crate::{Client, ImagePruneOutput};

#[derive(Debug, PartialEq, Args)]
pub(crate) struct PruneArgs {
    /// Prune without asking.
    #[arg(short, long)]
    pub(crate) force: bool,
}

/// Delete what no ref points at and no sandbox uses, after asking, and say what went.
pub(crate) async fn prune(client: &Client, args: PruneArgs) -> anyhow::Result<()> {
    if !args.force && !confirm("remove every image no ref points at and no sandbox uses?")? {
        return Ok(());
    }
    print!("{}", report(&client.prune_image().await?));
    Ok(())
}

fn report(pruned: &ImagePruneOutput) -> String {
    let mut out = String::new();
    for digest in &pruned.images {
        out.push_str(&format!("Deleted: {digest}\n"));
    }
    out.push_str(&format!(
        "Total reclaimed space: {}\n",
        size(pruned.reclaimed)
    ));
    out
}

/// `bytes` in decimal units, as `docker` shows a size.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.2}{}", UNITS[unit])
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;

    #[test]
    fn a_prune_says_each_image_it_deleted_and_the_space_freed() {
        let pruned = ImagePruneOutput {
            images: vec!["sha256:0".into()],
            reclaimed: 1024,
        };
        assert_eq!(
            report(&pruned),
            "Deleted: sha256:0\nTotal reclaimed space: 1.02kB\n"
        );
    }
}
