//! `build`: a Dockerfile into an image.

use std::{
    io::Read as _,
    path::{Path, PathBuf},
};

use crate::{Client, Recipe};
use anyhow::Context as _;
use clap::Args;

use crate::cli::{job, name::with_tag};

#[derive(Debug, PartialEq, Args)]
pub(crate) struct BuildArgs {
    /// The ref to tag the build with, as `name:tag`.
    #[arg(short = 't', long = "tag", value_name = "NAME[:TAG]")]
    pub(crate) tag: String,

    /// The Dockerfile to build; `PATH/Dockerfile` if absent, stdin if `-`.
    #[arg(short = 'f', long = "file", value_name = "FILE")]
    pub(crate) file: Option<PathBuf>,

    /// Print only the ref and digest, not the build's log.
    #[arg(short, long)]
    pub(crate) quiet: bool,

    /// The directory the Dockerfile is read from; `-` reads the Dockerfile from stdin.
    #[arg(default_value = ".", value_name = "PATH")]
    pub(crate) path: PathBuf,
}

/// Build a Dockerfile, read from a file or stdin, and print the ref and the digest it built.
pub(crate) async fn build(client: &Client, args: BuildArgs) -> anyhow::Result<()> {
    let (content, source) = match dockerfile(args.file.as_deref(), &args.path) {
        None => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("reading the Dockerfile from stdin")?;
            (text, "stdin".to_string())
        }
        Some(file) => (
            std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?,
            file.display().to_string(),
        ),
    };
    let recipe = Recipe::from_dockerfile(content).with_context(|| format!("in {source}"))?;
    let reference = with_tag(&args.tag);
    let job = client.build_image(recipe, &reference).await?;
    let digest = job::follow(client, &job, args.quiet).await?;
    println!("{reference} {digest}");
    Ok(())
}

/// Where the Dockerfile is: `file` if given, else `Dockerfile` under `path`; `None` is stdin,
/// which `-` names in either place, as `docker build -f -` and `docker build -` do.
fn dockerfile(file: Option<&Path>, path: &Path) -> Option<PathBuf> {
    let stdin = Path::new("-");
    match file {
        Some(file) if file == stdin => None,
        Some(file) => Some(file.to_path_buf()),
        None if path == stdin => None,
        None => Some(path.join("Dockerfile")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dash_reads_the_dockerfile_from_stdin() {
        assert_eq!(
            dockerfile(None, Path::new("app")),
            Some(PathBuf::from("app/Dockerfile"))
        );
        assert_eq!(
            dockerfile(Some(Path::new("ci/Dockerfile")), Path::new("app")),
            Some(PathBuf::from("ci/Dockerfile"))
        );
        assert_eq!(dockerfile(Some(Path::new("-")), Path::new("app")), None);
        assert_eq!(dockerfile(None, Path::new("-")), None);
        // A file named outright wins over a `-` path, as in docker.
        assert_eq!(
            dockerfile(Some(Path::new("ci/Dockerfile")), Path::new("-")),
            Some(PathBuf::from("ci/Dockerfile"))
        );
    }
}
