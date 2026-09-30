//! Building an image: booting the base, running what a recipe declares, and keeping what
//! the boot wrote.
//!
//! # Why this is here and not in the library
//!
//! Because a build is a machine, and a machine is a *process* here. Creating one on macOS
//! needs an entitlement, an entitlement is carried by a code signature, and a signature is
//! read at `exec` -- so the thing that makes a VM is `cortex-krun-host`, spawned as a file
//! beside this binary. `cortex-krun-lib` holds everything a build needs except the one
//! thing it cannot be: something that spawns that file and lives to read the result.
//!
//! So the split is the same one the rest of this server has. The library knows what a
//! layer is, what a cache is, and how to read a session's disk back out
//! ([`Ext4::to_blob`]) and file the result ([`commit`]). This module knows how to get a
//! machine to do some work first.
//!
//! # What a build is
//!
//! One boot, and one layer over the base. Not one layer per step: a step is cheap and a
//! boot is not, so every `RUN` a recipe declares runs on one machine and what they all
//! left behind is taken together.
//!
//! ```text
//! recipe ── base ──► pull, provision ──► disk
//!        └─ steps ──► RUN      ──► one shell line each, in order
//!                     ENV      ──► exported before the RUNs after it, and stated
//!                     WORKDIR  ──► cd'd into before the RUNs after it, and stated
//!
//!   boot(disk, a fresh ext4) ──► exec each line ──► sync ──► kill
//!                                                            │
//!                            the ext4 ── to_blob ──► commit ─┘
//! ```
//!
//! # Naming, and why a second build of the same recipe is free
//!
//! An image built here is filed under the digest of **what was declared** -- the base's
//! manifest and the steps as the caller wrote them. So the second `init` naming the same
//! recipe finds the image in the index and never boots anything. That is also why the
//! steps are hashed in their declared form rather than as the shell lines they become:
//! two callers who declared the same thing get the same image whatever this module does
//! with it on the way.

use cortex::image::{Recipe, Step};
use cortex::protocol::{Call, Error, ExecCall, InitCall, Response};
use cortex_krun::image::{Cache, Digest, Image, ImageState, commit};
use cortex_krun::session::Ext4;
use cortex_krun_host::{ABIN_DIR, GUEST_DIR, guest_program};

use std::io::{Read as _, Seek as _, Write as _};
use std::path::Path;
use std::time::Duration;

use super::machine::{Doors, Machine, Shape};
use super::{SYNC, coded, pulled};

/// How a build's name is spelled, so that a change to what goes into the digest cannot be
/// read as an image somebody once built.
const RECIPE: &str = "cortex-krun/build/1\n";

/// Where a built image is filed. Not a registry anybody can reach: an image made here has
/// no home but this cache, and a name that looked pullable would be a lie.
const REPOSITORY: &str = "localhost/cortex-krun-build";

const GUEST_CONSOLE: &str = "/dev/console";

/// Whether `name` is one this module filed a build under, rather than one a client chose.
pub fn internal(name: &str) -> bool {
    name.strip_prefix(REPOSITORY)
        .is_some_and(|rest| rest.starts_with(':'))
}

/// Build what `recipe` declares, and answer with the name the built image is filed under,
/// and whether this call is what filed it -- `false` when the same recipe was already there.
///
/// That name is what a session then boots on, so a caller treats the answer exactly as it
/// would have treated `recipe.base` had there been no steps.
///
/// A build has a network whatever the session it is for asked for. It has to: the commonest
/// first step of any recipe fetches packages, and a session that is to run offline still has
/// to be built by something that can reach a mirror. No published ports, because a build
/// serves nothing.
///
/// Nothing is booted for a recipe whose steps are all `ENV` and `WORKDIR`. Those change
/// what the image states and not what is in it, and there is nothing for a machine to do.
pub async fn build(recipe: &Recipe) -> anyhow::Result<(String, bool)> {
    // The base has to be in the cache and stitchable before anything can boot on it, and a
    // name nobody can pull is a failure about what was asked for rather than about this
    // machine -- which is why it is answered here, before any of the rest.
    pulled(&recipe.base).await?;

    let cache = Cache::new();
    let base = based(&recipe.base, &cache)?;
    let named = name(&base, recipe)?;

    // The same recipe over the same base is the same image. Answered before a machine is
    // made, which is the whole reason the name is the digest of the declaration.
    if cache.image_index()?.contains_key(&named) {
        return Ok((named, false));
    }

    let (script, config) = plan(recipe, &base.config)?;

    if script.is_empty() {
        commit(&base, None, config, named.as_str(), Some(cache))
            .map_err(|e| coded(Error::INTERNAL_ERROR, anyhow::Error::new(e)))?;
        return Ok((named, true));
    }

    // The disk the build writes on. Its own, and never the session's: what a session wrote
    // is not what this recipe declared, and a layer holding both would be an image nobody
    // asked for. Under this process's directory, so a server that dies leaves it for the
    // next one's sweep.
    let at = super::sessions().join(std::process::id().to_string());
    std::fs::create_dir_all(&at)?;
    // The operator's size and not the session's, for the reason the machine's shape is --
    // see [`run`].
    let upper = Ext4::at(at.join("build.ext4"), super::machine::disk_from_env())?;
    let blob = at.join("build.tar");

    run(&recipe.base, &upper, &script).await?;

    // Only now that the machine is down and has flushed -- `run` took it down on its way
    // out, and the last thing it asked the guest for was a flush. Saying so is this end's
    // to do: the disk was never unmounted, because it was the root.
    upper.settled()?;

    let written = std::fs::File::create(&blob)?;
    let diff_id = upper.to_blob(&[GUEST_DIR, ABIN_DIR], &written)?;
    written.sync_all()?;
    drop(written);

    let built = commit(
        &base,
        Some((&blob, &diff_id)),
        config,
        named.as_str(),
        Some(cache),
    );

    // Both are this build's alone and neither is worth keeping: the layer is in the cache
    // by now, and the disk it came off is a filesystem nothing will mount again.
    let _ = std::fs::remove_file(&blob);
    let _ = std::fs::remove_file(&upper.path);

    built.map_err(|e| coded(Error::INTERNAL_ERROR, anyhow::Error::new(e)))?;
    Ok((named, true))
}

/// Boot on `base`, run every line of `script` in order, and flush before coming back.
///
/// The machine is dropped on the way out however this leaves, which kills it -- so a step
/// that failed takes the machine with it and the disk is left holding whatever the steps
/// before it wrote.
async fn run(base: &str, upper: &Ext4, script: &[String]) -> anyhow::Result<()> {
    // Half the host and no GPU, not the session's shape -- see [`Shape::for_build`]. A network
    // always, because a recipe's commonest first step fetches packages -- a build that could
    // not reach a registry would be a different build, where one that is smaller is the same
    // one, slower.
    let mut machine = Machine::boot(
        base,
        &upper.path,
        &[],
        true,
        &Doors::default(),
        Shape::for_build(),
    )
    .await
    .map_err(|e| coded(Error::BOOT_FAILED, e))?;

    // The guest answers nothing until it has been told what the session is. A build's
    // session has no trees in it: what a step may read is what the image shipped and what
    // an earlier step wrote, and a share would put this host's files in the image.
    match machine.call(Call::Init(InitCall::default())).await? {
        Response::Init(_) => {}
        Response::Error(e) => anyhow::bail!("the guest refused the build: {}", e.message),
        other => anyhow::bail!("the guest answered an init with {other:?}"),
    }

    let mut console = Console::open(&machine.console())?;
    console.skip()?;

    for (index, line) in script.iter().enumerate() {
        console.drain();
        eprintln!("step {}/{}: {line}", index + 1, script.len());

        let answered = console
            .during(machine.call(Call::Exec(ExecCall {
                // Through `sh`, which is what a `RUN` is declared to be -- see the
                // protocol's own note on the step. No timeout: a build has no deadline
                // this end could invent that would not be wrong for somebody.
                cmd: vec!["sh".into(), "-c".into(), shown(line, GUEST_CONSOLE)],
                timeout_ms: None,
            })))
            .await?;
        console.drain();

        match answered {
            Response::Exec(done) if done.code == 0 => {}
            Response::Exec(done) => {
                // What the step said, because that is the whole of why it failed and the
                // client has no other way to see it -- the machine is about to be killed.
                let said = String::from_utf8_lossy(&done.stderr);
                let said = said.trim();
                return Err(coded(
                    Error::UNSUPPORTED_IMAGE,
                    anyhow::anyhow!(
                        "step {} exited {}{}",
                        index + 1,
                        done.code,
                        if said.is_empty() {
                            String::new()
                        } else {
                            format!(": {said}")
                        }
                    ),
                ));
            }
            Response::Error(e) => anyhow::bail!("step {}: {}", index + 1, e.message),
            other => anyhow::bail!("the guest answered step {} with {other:?}", index + 1),
        }
    }

    // **The one call that is not the recipe's.** What the guest wrote is on its way to the
    // disk and not necessarily on it; the machine is stopped by killing it, which flushes
    // nothing; and the file is about to be read on this side.
    match machine
        .call(Call::Exec(ExecCall {
            cmd: vec![guest_program(), SYNC.into()],
            timeout_ms: None,
        }))
        .await?
    {
        Response::Exec(done) if done.code == 0 => Ok(()),
        Response::Exec(done) => {
            anyhow::bail!("flushing the build's disk exited {}", done.code)
        }
        Response::Error(e) => anyhow::bail!("flushing the build's disk: {}", e.message),
        other => anyhow::bail!("the guest answered a flush with {other:?}"),
    }
}

struct Console(std::fs::File);

impl Console {
    fn open(path: &Path) -> anyhow::Result<Console> {
        let file = std::fs::File::open(path)
            .map_err(|e| anyhow::anyhow!("opening {}: {e}", path.display()))?;
        Ok(Console(file))
    }

    fn skip(&mut self) -> std::io::Result<()> {
        self.0.seek(std::io::SeekFrom::End(0)).map(|_| ())
    }

    fn drain(&mut self) {
        let mut read = Vec::new();
        if self.0.read_to_end(&mut read).is_ok() && !read.is_empty() {
            let _ = std::io::stderr().write_all(&read);
        }
    }

    async fn during<T>(&mut self, work: impl std::future::Future<Output = T>) -> T {
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        tokio::pin!(work);
        loop {
            tokio::select! {
                done = &mut work => return done,
                _ = tick.tick() => self.drain(),
            }
        }
    }
}

fn shown(line: &str, console: &str) -> String {
    format!(
        "rc=$( {{ {{ ( {line}\n) 3>&-; echo $? >&3; }} 2>&1 >>{console} | tee -a {console} >&2; }} 3>&1 ); exit ${{rc:-1}}"
    )
}

/// What a recipe's steps come to: the shell lines to run, and what the image ends up
/// stating about running.
///
/// `ENV` and `WORKDIR` are both, which is what makes this one pass and not two. They change
/// what the image states *and* how every later `RUN` is spelled -- a variable is exported
/// in front of the commands after it, and a directory is `cd`'d into before them.
///
/// The environment the *base* states is not exported here. The guest's init already
/// started the guest with it, and a command the guest spawns inherits it -- so exporting it
/// again would be this end repeating something that is already true.
fn plan(recipe: &Recipe, base: &ImageState) -> anyhow::Result<(Vec<String>, ImageState)> {
    let mut config = base.clone();
    let mut script = Vec::new();
    // Only what the recipe added, in declaration order, which is what a later `RUN` needs
    // in front of it.
    let mut exported: Vec<(String, String)> = Vec::new();
    let mut workdir = base.working_dir.clone();

    for step in &recipe.steps {
        match step {
            Step::Run(command) => {
                anyhow::ensure!(!command.trim().is_empty(), "a RUN step names no command");
                script.push(line(&workdir, &exported, command));
            }

            Step::Env { key, value } => {
                anyhow::ensure!(
                    !key.is_empty() && !key.contains(['=', ' ']),
                    "{key:?} is not a name a variable can have"
                );
                set(&mut exported, key, value);
                let stated = config.env.get_or_insert_with(Vec::new);
                stated.retain(|pair| !pair.starts_with(&format!("{key}=")));
                stated.push(format!("{key}={value}"));
            }

            Step::Workdir(dir) => {
                anyhow::ensure!(!dir.is_empty(), "a WORKDIR step names no directory");
                workdir = Some(dir.clone());
                config.working_dir = Some(dir.clone());
            }

            // **The one step this server has no way to realize.** `src` is a path in a
            // build context, and the protocol carries no context: `init` says what the
            // image is and where the session's trees are, and neither of those is a
            // directory the copy is to be read from. There is nothing to guess at here --
            // any directory this picked would be this host's and not the caller's.
            Step::Copy { src, dst } => {
                return Err(coded(
                    Error::UNSUPPORTED_IMAGE,
                    anyhow::anyhow!(
                        "COPY {} {dst}: a copy reads from a build context, and an init \
                         carries none -- so there is no directory this server could read \
                         {} out of",
                        src.display(),
                        src.display()
                    ),
                ));
            }
        }
    }

    Ok((script, config))
}

/// One `RUN`, as the line a shell is handed.
fn line(workdir: &Option<String>, exported: &[(String, String)], command: &str) -> String {
    let mut line = String::new();

    if let Some(dir) = workdir {
        line.push_str(&format!("cd {} && ", quote(dir)));
    }
    if !exported.is_empty() {
        line.push_str("export");
        for (key, value) in exported {
            // The key unquoted because it is a name and has been checked as one; the value
            // quoted because it is anything at all.
            line.push_str(&format!(" {key}={}", quote(value)));
        }
        line.push_str(" && ");
    }
    line.push_str(command);
    line
}

/// `text`, so that a shell reads it as exactly these bytes.
///
/// Single quotes, which is the one quoting in `sh` with no escapes inside it -- so the
/// only thing to handle is a single quote itself, which ends the string, is escaped
/// outside it, and starts a new one.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Add or replace a variable, keeping the order the recipe declared them in.
fn set(exported: &mut Vec<(String, String)>, key: &str, value: &str) {
    match exported.iter_mut().find(|(name, _)| name == key) {
        Some(existing) => existing.1 = value.to_string(),
        None => exported.push((key.to_string(), value.to_string())),
    }
}

/// The image a recipe builds over, which a pull has just made sure is here.
fn based(base: &str, cache: &Cache) -> anyhow::Result<Image> {
    let reference: cortex_krun::image::Reference = base
        .parse()
        .map_err(|e| coded(Error::UNKNOWN_IMAGE, anyhow::anyhow!("{base}: {e}")))?;
    let name = reference.to_string();

    cache.image_index()?.get(&name).cloned().ok_or_else(|| {
        coded(
            Error::UNKNOWN_IMAGE,
            anyhow::anyhow!("{name} is not in the cache, having just been pulled into it"),
        )
    })
}

/// What a built image is filed under: the digest of what was declared.
///
/// The base by the manifest it resolved to rather than by the string that named it, so
/// that a tag somebody moved is a different build -- and the steps by their declared form,
/// which is what [`Step`]'s own `Display` is for.
fn name(base: &Image, recipe: &Recipe) -> anyhow::Result<String> {
    let mut text = String::from(RECIPE);
    text.push_str(&base.manifest.to_string());
    text.push('\n');
    for step in &recipe.steps {
        text.push_str(&step.to_string());
        text.push('\n');
    }

    let named = format!("{REPOSITORY}:{}", Digest::of(text).hex());
    // Through a `Reference` and back, because the index is keyed by the canonical spelling
    // and a lookup against anything else would miss what this very function filed.
    let reference: cortex_krun::image::Reference = named
        .parse()
        .map_err(|e| anyhow::anyhow!("{named} is not a name an image can have: {e}"))?;
    Ok(reference.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(steps: Vec<Step>) -> Recipe {
        Recipe::new("alpine:3.21").steps(steps)
    }

    /// The two steps that are not commands still reach every command after them, and only
    /// the ones after them.
    #[test]
    fn env_and_workdir_apply_to_the_steps_that_follow() {
        let declared = recipe(vec![
            Step::run("first"),
            Step::env("TZ", "UTC"),
            Step::workdir("/work"),
            Step::run("second"),
        ]);

        let (script, config) = plan(&declared, &ImageState::default()).unwrap();

        assert_eq!(script.len(), 2);
        assert_eq!(script[0], "first", "a later ENV reached an earlier RUN");
        assert_eq!(script[1], "cd '/work' && export TZ='UTC' && second");

        assert_eq!(config.working_dir.as_deref(), Some("/work"));
        assert_eq!(config.env.as_deref(), Some(&["TZ=UTC".to_string()][..]));
    }

    /// A value is anything at all, and a shell must read it as exactly what was declared.
    #[test]
    fn a_value_cannot_escape_the_line_it_is_in() {
        let declared = recipe(vec![
            Step::env("EVIL", "'; rm -rf /; echo '"),
            Step::run("echo $EVIL"),
        ]);

        let (script, _) = plan(&declared, &ImageState::default()).unwrap();
        assert_eq!(
            script[0],
            r"export EVIL=''\''; rm -rf /; echo '\''' && echo $EVIL"
        );
    }

    /// Restating a variable replaces it rather than stacking a second one an image would
    /// have to pick between.
    #[test]
    fn a_variable_stated_twice_is_one_variable() {
        let declared = recipe(vec![
            Step::env("TZ", "UTC"),
            Step::env("TZ", "Asia/Seoul"),
            Step::run("date"),
        ]);

        let (script, config) = plan(&declared, &ImageState::default()).unwrap();
        assert_eq!(script[0], "export TZ='Asia/Seoul' && date");
        assert_eq!(
            config.env.as_deref(),
            Some(&["TZ=Asia/Seoul".to_string()][..])
        );
    }

    /// The base's own environment is already in the machine, and repeating it here would
    /// be this end asserting something it did not learn.
    #[test]
    fn what_the_base_states_is_not_exported_again() {
        let base = ImageState {
            env: Some(vec!["PATH=/usr/bin".to_string()]),
            working_dir: Some("/srv".to_string()),
            ..Default::default()
        };
        let (script, config) = plan(&recipe(vec![Step::run("build")]), &base).unwrap();

        assert_eq!(script[0], "cd '/srv' && build", "{script:?}");
        assert_eq!(
            config.env.as_deref(),
            Some(&["PATH=/usr/bin".to_string()][..])
        );
    }

    /// Nothing to run is nothing to boot, and the caller reads that off an empty script.
    #[test]
    fn a_recipe_of_statements_alone_runs_nothing() {
        let declared = recipe(vec![Step::env("TZ", "UTC"), Step::workdir("/work")]);
        let (script, config) = plan(&declared, &ImageState::default()).unwrap();

        assert!(script.is_empty(), "{script:?}");
        assert_eq!(config.working_dir.as_deref(), Some("/work"));
    }

    fn ran(line: &str) -> (i32, String, String) {
        let dir = std::env::temp_dir().join(format!("cortex-krun-shown-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let console = dir.join(format!("{}.log", line.len()));
        std::fs::write(&console, b"").unwrap();
        let out = std::process::Command::new("sh")
            .args(["-c", &shown(line, console.to_str().unwrap())])
            .output()
            .unwrap();
        let shown = std::fs::read_to_string(&console).unwrap();
        let _ = std::fs::remove_file(&console);
        (
            out.status.code().unwrap(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            shown,
        )
    }

    #[test]
    fn a_step_is_shown_and_keeps_its_code_and_stderr() {
        let (code, stderr, shown) = ran("echo out; echo err >&2; exit 7");
        assert_eq!(code, 7);
        assert_eq!(stderr, "err\n");
        assert!(
            shown.contains("out\n") && shown.contains("err\n"),
            "{shown:?}"
        );

        let (code, _, shown) = ran("echo fine # a trailing comment");
        assert_eq!(code, 0);
        assert_eq!(shown, "fine\n");

        let (code, _, _) = ran("false");
        assert_eq!(code, 1);
    }

    /// Refused where it is read, and by a code that says the image is what has to change.
    #[test]
    fn a_copy_is_refused_for_want_of_a_context() {
        let declared = recipe(vec![Step::copy("app", "/srv/app")]);
        let refused = plan(&declared, &ImageState::default()).unwrap_err();

        assert_eq!(
            super::super::refused(&refused).code,
            Error::UNSUPPORTED_IMAGE
        );
        assert!(
            format!("{refused:#}").contains("build context"),
            "{refused:#}"
        );
    }
}
