use std::time::Duration;

use clap::Subcommand;
use cortex_krun::image::{self, GRACE};

#[derive(Subcommand)]
pub(crate) enum Command {
    /// List the images in the cache.
    #[command(visible_alias = "list")]
    Ls,
    /// Pull an image from its registry into the cache.
    Pull { reference: String },
    /// Give an image in the cache another name.
    Tag { from: String, to: String },
    /// Take a name away, leaving the image it pointed at.
    Untag { reference: String },
    /// Remove an image, every name it has and the files only it uses.
    #[command(visible_alias = "remove")]
    Rm {
        #[arg(required = true)]
        references: Vec<String>,
    },
    /// Delete every file in the cache that no image names.
    Prune {
        /// Also delete files written in the last hour, which a pull may not have named yet.
        #[arg(long)]
        all: bool,
    },
}

pub(crate) async fn run(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Ls => {
            let listed = image::list(None)?;
            let width = listed
                .iter()
                .map(|entry| entry.reference.len())
                .max()
                .unwrap_or(0)
                .max("REFERENCE".len());
            println!(
                "{:width$}  {:12}  {:>9}  PROVISIONED",
                "REFERENCE", "MANIFEST", "SIZE"
            );
            for entry in listed {
                let manifest = entry.image.manifest.hex();
                println!(
                    "{:width$}  {:12}  {:>9}  {}",
                    entry.reference,
                    &manifest[..manifest.len().min(12)],
                    bytes(entry.bytes),
                    if entry.provisioned { "yes" } else { "no" },
                );
            }
        }
        Command::Pull { reference } => {
            let pulled = image::pull(reference.as_str(), None, None).await?;
            println!("{reference}: {}", pulled.manifest);
        }
        Command::Tag { from, to } => {
            let tagged = image::tag(from.as_str(), to.as_str(), None)?;
            println!("{to}: {}", tagged.manifest);
        }
        Command::Untag { reference } => {
            let untagged = image::untag(reference.as_str(), None)?;
            println!("untagged {reference} ({})", untagged.manifest);
        }
        Command::Rm { references } => {
            for reference in references {
                let removed = image::remove(reference.as_str(), None)?;
                for name in &removed.untagged {
                    println!("untagged {name}");
                }
                println!("removed {reference}, reclaiming {}", bytes(removed.bytes));
            }
        }
        Command::Prune { all } => {
            let grace = if all { Duration::ZERO } else { GRACE };
            let pruned = image::prune(Some(grace), None)?;
            println!(
                "deleted {} files, reclaiming {}",
                pruned.files.len(),
                bytes(pruned.bytes)
            );
            if pruned.spared > 0 {
                println!(
                    "spared {} written in the last hour; --all deletes them too",
                    pruned.spared
                );
            }
        }
    }
    Ok(())
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
