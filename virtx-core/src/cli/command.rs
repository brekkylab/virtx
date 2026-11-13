use std::ffi::OsString;

use clap::{Parser, Subcommand};

use crate::Client;
use crate::cli::{
    build::{self, BuildArgs},
    cloud::{self, CloudArgs, CloudCommand},
    inspect::{self, InspectArgs},
    job::Interrupted,
    load::{self, LoadArgs},
    ls::{self, LsArgs},
    prune::{self, PruneArgs},
    pull::{self, PullArgs},
    push::{self, PushArgs},
    rm::{self, RmArgs},
    save::{self, SaveArgs},
    server::{self, ServerArgs},
    tag::{self, TagArgs},
};

#[derive(Debug, Parser)]
#[command(
    name = "virtx",
    version,
    about = "Build and manage a virtx server's images, and sign in to virtx cloud"
)]
pub(crate) struct Cli {
    #[command(flatten)]
    server: ServerArgs,

    #[command(flatten)]
    cloud: CloudArgs,

    #[command(subcommand)]
    command: Command,
}

// What each command does, said once for both spellings.
const BUILD: &str = "Build an image from a Dockerfile";
const LS: &str = "List images";
const INSPECT: &str = "Print images as JSON, each by a ref (`name:tag`), image ID or digest";
const RM: &str = "Untag refs (`name:tag`), removing an image with its last ref, or remove \
                  images by image ID or digest with every ref on them";
const PULL: &str = "Pull an image from a registry, tagged as it is named";
const PUSH: &str = "Push an image to the registry its ref names";
const SAVE: &str = "Write an image to an OCI archive";
const LOAD: &str = "Tag the image in an OCI archive, uploading what the server lacks";
const TAG: &str = "Point a ref at an image, moving the ref if it is in use";

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage images: the commands below, as `image <COMMAND>`.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },

    #[command(about = BUILD)]
    Build(BuildArgs),

    #[command(about = LS)]
    Images(LsArgs),

    #[command(about = INSPECT)]
    Inspect(InspectArgs),

    #[command(about = RM)]
    Rmi(RmArgs),

    #[command(about = PULL)]
    Pull(PullArgs),

    #[command(about = PUSH)]
    Push(PushArgs),

    #[command(about = TAG)]
    Tag(TagArgs),

    #[command(about = SAVE)]
    Save(SaveArgs),

    #[command(about = LOAD)]
    Load(LoadArgs),

    /// virtx cloud's: `login`, `token`, `whoami` and `logout`.
    #[command(flatten)]
    Cloud(CloudCommand),
}

impl Command {
    /// The image command it is; `Err` with virtx cloud's, which reaches no server.
    fn into_image(self) -> Result<ImageCommand, CloudCommand> {
        Ok(match self {
            Command::Cloud(command) => return Err(command),
            Command::Image { command } => command,
            Command::Build(args) => ImageCommand::Build(args),
            Command::Images(args) => ImageCommand::Ls(args),
            Command::Inspect(args) => ImageCommand::Inspect(args),
            Command::Rmi(args) => ImageCommand::Rm(args),
            Command::Pull(args) => ImageCommand::Pull(args),
            Command::Push(args) => ImageCommand::Push(args),
            Command::Tag(args) => ImageCommand::Tag(args),
            Command::Save(args) => ImageCommand::Save(args),
            Command::Load(args) => ImageCommand::Load(args),
        })
    }
}

#[derive(Debug, PartialEq, Subcommand)]
enum ImageCommand {
    #[command(about = BUILD)]
    Build(BuildArgs),

    #[command(about = LS)]
    Ls(LsArgs),

    #[command(about = INSPECT)]
    Inspect(InspectArgs),

    #[command(about = RM)]
    Rm(RmArgs),

    #[command(about = PULL)]
    Pull(PullArgs),

    #[command(about = PUSH)]
    Push(PushArgs),

    #[command(about = TAG)]
    Tag(TagArgs),

    #[command(about = SAVE)]
    Save(SaveArgs),

    #[command(about = LOAD)]
    Load(LoadArgs),

    /// Remove every image no ref points at and no sandbox uses.
    Prune(PruneArgs),
}

/// Run the command line `argv` spells, the program name first, and answer the exit code: 0 when
/// done, 1 when a command failed, 130 when Ctrl-C cancelled it, and clap's own for an argument it
/// could not read or a `--help`.
pub(crate) async fn run(argv: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> u8 {
    // Rust starts with SIGPIPE ignored, so `virtx images | head` would fail on the write after
    // `head` exits, where a shell tool ends quietly.
    #[cfg(unix)]
    // SAFETY: `signal` is process-wide and may be called from any thread at any time.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return u8::try_from(error.exit_code()).unwrap_or(2);
        }
    };
    match execute(cli).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("virtx: {}", report(&error));
            if error.is::<Interrupted>() { 130 } else { 1 }
        }
    }
}

async fn execute(cli: Cli) -> anyhow::Result<()> {
    let command = match cli.command.into_image() {
        Ok(command) => command,
        // virtx cloud's commands start no server and reach none.
        Err(command) => return cloud::run(&cli.cloud.domain, command).await,
    };
    let client = server::connect(&cli.server).await?;
    let done = dispatch(&client, command).await;
    client.close().await;
    done
}

async fn dispatch(client: &Client, command: ImageCommand) -> anyhow::Result<()> {
    match command {
        ImageCommand::Build(args) => build::build(client, args).await,
        ImageCommand::Ls(args) => ls::ls(client, args).await,
        ImageCommand::Inspect(args) => inspect::inspect(client, args).await,
        ImageCommand::Rm(args) => rm::rm(client, args).await,
        ImageCommand::Pull(args) => pull::pull(client, args).await,
        ImageCommand::Push(args) => push::push(client, args).await,
        ImageCommand::Tag(args) => tag::tag(client, args).await,
        ImageCommand::Save(args) => save::save(client, args).await,
        ImageCommand::Load(args) => load::load(client, args).await,
        ImageCommand::Prune(args) => prune::prune(client, args).await,
    }
}

/// The error and its causes, as `cause: cause`, each cause its parent already spells skipped.
pub(crate) fn report(error: &anyhow::Error) -> String {
    let mut text = String::new();
    for cause in error.chain().map(ToString::to_string) {
        if text.ends_with(&cause) {
            continue;
        }
        if !text.is_empty() {
            text.push_str(": ");
        }
        text.push_str(&cause);
    }
    text
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::CommandFactory as _;

    use super::*;

    fn parse(args: &[&str]) -> ImageCommand {
        let mut argv = vec!["virtx"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv)
            .unwrap()
            .command
            .into_image()
            .unwrap()
    }

    #[test]
    fn build_takes_tag_file_and_context() {
        assert_eq!(
            parse(&[
                "image",
                "build",
                "-t",
                "jq:1",
                "-f",
                "ci/Dockerfile",
                "-q",
                "app"
            ]),
            ImageCommand::Build(BuildArgs {
                tag: "jq:1".into(),
                file: Some("ci/Dockerfile".into()),
                quiet: true,
                path: "app".into(),
            })
        );
        assert_eq!(
            parse(&["image", "build", "-t", "jq"]),
            ImageCommand::Build(BuildArgs {
                tag: "jq".into(),
                file: None,
                quiet: false,
                path: Path::new(".").into(),
            })
        );
        // The server takes no build without a ref to tag it with.
        assert!(Cli::try_parse_from(["virtx", "build", "."]).is_err());
    }

    #[test]
    fn rm_needs_at_least_one_image() {
        assert!(Cli::try_parse_from(["virtx", "image", "rm"]).is_err());
        assert_eq!(
            parse(&["image", "rm", "-f", "a:1", "sha256:ab"]),
            ImageCommand::Rm(RmArgs {
                images: vec!["a:1".into(), "sha256:ab".into()],
                force: true
            })
        );
    }

    #[test]
    fn both_spellings_are_the_same_command() {
        let pairs: [(&[&str], &[&str]); 9] = [
            (
                &["build", "-t", "a:1", "dir"],
                &["image", "build", "-t", "a:1", "dir"],
            ),
            (&["images", "-q"], &["image", "ls", "-q"]),
            (&["inspect", "a:1"], &["image", "inspect", "a:1"]),
            (&["rmi", "-f", "a:1"], &["image", "rm", "-f", "a:1"]),
            (&["pull", "alpine:3.20"], &["image", "pull", "alpine:3.20"]),
            (&["tag", "a:1", "b"], &["image", "tag", "a:1", "b"]),
            (
                &["push", "-u", "me", "a:1"],
                &["image", "push", "-u", "me", "a:1"],
            ),
            (
                &["save", "-o", "a.tar", "a:1"],
                &["image", "save", "-o", "a.tar", "a:1"],
            ),
            (
                &["load", "-i", "a.tar", "-t", "a:1"],
                &["image", "load", "-i", "a.tar", "-t", "a:1"],
            ),
        ];
        for (alias, command) in pairs {
            assert_eq!(parse(alias), parse(command), "{alias:?}");
        }
    }

    #[test]
    fn prune_is_an_image_command_alone() {
        assert_eq!(
            parse(&["image", "prune", "-f"]),
            ImageCommand::Prune(PruneArgs { force: true })
        );
        assert!(Cli::try_parse_from(["virtx", "prune"]).is_err());
    }

    #[test]
    fn a_host_and_a_server_to_start_are_one_or_the_other() {
        let cli = Cli::try_parse_from(["virtx", "-H", "https://x", "images"]).unwrap();
        assert_eq!(cli.server.host.as_deref(), Some("https://x"));
        assert!(
            Cli::try_parse_from(["virtx", "-H", "https://x", "--server", "./s", "images"]).is_err()
        );
    }

    #[test]
    fn the_server_is_named_before_or_after_the_command() {
        let before = Cli::try_parse_from(["virtx", "--server", "./s", "images"]).unwrap();
        let after = Cli::try_parse_from(["virtx", "images", "--server", "./s"]).unwrap();
        assert_eq!(before.server, after.server);
        assert_eq!(before.server.server.as_deref(), Some(Path::new("./s")));
    }

    /// virtx cloud's commands are commands of the one parser (issues #158, #178): `--domain`
    /// before the command as after it, and `--help` lists them.
    #[test]
    fn virtx_cloud_s_commands_take_their_domain_anywhere_and_are_listed() {
        for argv in [
            &["virtx", "--domain", "example.invalid", "whoami"][..],
            &["virtx", "whoami", "--domain", "example.invalid"],
            &["virtx", "--domain=example.invalid", "whoami"],
        ] {
            let cli = Cli::try_parse_from(argv).unwrap();
            assert_eq!(cli.cloud.domain, "example.invalid", "{argv:?}");
            assert_eq!(
                cli.command.into_image().err(),
                Some(CloudCommand::Whoami),
                "{argv:?}"
            );
        }
        let cli = Cli::try_parse_from(["virtx", "token", "--json"]).unwrap();
        assert_eq!(cli.cloud.domain, cloud::DOMAIN);
        assert_eq!(
            cli.command.into_image().err(),
            Some(CloudCommand::Token { json: true })
        );
        let help = Cli::command().render_help().to_string();
        for command in ["login", "token", "whoami", "logout", "build", "images"] {
            assert!(help.contains(command), "{command} in:\n{help}");
        }
    }

    #[test]
    fn a_report_spells_each_cause_once() {
        let error = anyhow::anyhow!("inner").context("outer");
        assert_eq!(report(&error), "outer: inner");
        let repeated = anyhow::anyhow!("no such image").context("removing: no such image");
        assert_eq!(report(&repeated), "removing: no such image");
    }
}
