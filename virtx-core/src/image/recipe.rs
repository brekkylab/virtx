//! A build, declared: a base and the [`Step`]s over it.

use std::fmt;

use anyhow::Context as _;
use serde::{Deserialize, Deserializer, Serialize};

/// Which spelling of this format a declaration is written in.
///
/// Versions how a build is *written down*, not how it is named: a store digesting this type
/// may change its digest independently, without making either's stored values unreadable.
const FORMAT_VERSION: u32 = 1;

/// A base and the [`Step`]s over it: a declaration only, which each console server builds its
/// own way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    /// Serialized first, so a declaration from a newer virtx is refused by version rather than
    /// by whichever member it disagrees on.
    #[serde(rename = "v", deserialize_with = "known_version")]
    version: u32,

    /// The base, as the caller spelled it.
    ///
    /// A document with an empty base is refused on read, as there is no build without one.
    #[serde(deserialize_with = "some_base")]
    pub base: String,

    /// The steps, in the order they will run.
    pub steps: Vec<Step>,
}

impl Recipe {
    /// A declaration over `base`, with no steps yet.
    ///
    /// `base` is spelled as a registry does; an image built from nothing uses `scratch`, as in
    /// a Dockerfile. Prefer a digest to a tag: a store names this build by the declared string,
    /// not what it resolved to, so a moved tag keeps serving the image built before it moved.
    ///
    /// ```
    /// # use virtx_core::{Recipe, Step};
    /// let declared = Recipe::new("alpine:3.20")
    ///     .step("apk add --no-cache jq")
    ///     .step(Step::env("TZ", "UTC"));
    ///
    /// assert_eq!(declared.base, "alpine:3.20");
    /// assert_eq!(declared.steps.len(), 2);
    /// ```
    pub fn new(base: impl Into<String>) -> Self {
        Recipe {
            version: FORMAT_VERSION,
            base: base.into(),
            steps: Vec::new(),
        }
    }

    /// A declaration read from what a Dockerfile says.
    ///
    /// Takes text rather than a path, since its origin (a file, a request, a rendered template)
    /// is the caller's business; refusal line numbers point into the text given.
    ///
    /// One stage, and only `FROM`, `RUN`, `ENV` and `WORKDIR`. Anything else is refused at its
    /// line: an image with `CMD` or `USER` is not what this would build, and there is no side
    /// channel to report what was skipped.
    ///
    /// ```
    /// # use virtx_core::{Recipe, Step};
    /// let declared = Recipe::from_dockerfile("FROM alpine:3.20\nRUN apk add jq\n")?;
    ///
    /// assert_eq!(declared.base, "alpine:3.20");
    /// assert_eq!(declared.steps, [Step::run("apk add jq")]);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn from_dockerfile(content: impl AsRef<str>) -> anyhow::Result<Self> {
        // Lines into instructions: comments and blanks dropped, `\` continuations joined. A
        // comment inside a continuation is dropped without ending it, as `docker build` does
        // (annotated package lists rely on it). Collected first so a file ending
        // mid-continuation is one case here, not a second copy of the dispatch below.
        let mut instructions: Vec<(usize, String)> = Vec::new();
        let mut current: Option<(usize, String)> = None;
        for (index, raw) in content.as_ref().lines().enumerate() {
            let trimmed = raw.trim();
            if trimmed.starts_with('#') || (trimmed.is_empty() && current.is_none()) {
                continue;
            }
            let (body, continues) = match trimmed.strip_suffix('\\') {
                Some(body) => (body.trim_end(), true),
                None => (trimmed, false),
            };
            match current.as_mut() {
                Some((_, text)) if !body.is_empty() => {
                    text.push(' ');
                    text.push_str(body);
                }
                Some(_) => {}
                None => current = Some((index + 1, body.to_string())),
            }
            if !continues
                && let Some(instruction) = current.take()
                && !instruction.1.is_empty()
            {
                instructions.push(instruction);
            }
        }
        // Ended mid-continuation: keep it so it is still checked, not silently dropped.
        if let Some(instruction) = current.take() {
            instructions.push(instruction);
        }

        let mut base: Option<String> = None;
        let mut steps: Vec<Step> = Vec::new();
        for (line, text) in instructions {
            let (instruction, rest) = match text.split_once(char::is_whitespace) {
                Some((instruction, rest)) => (instruction.to_uppercase(), rest.trim()),
                None => (text.to_uppercase(), ""),
            };
            match instruction.as_str() {
                "FROM" => {
                    anyhow::ensure!(
                        base.is_none(),
                        "Dockerfile:{line}: a second FROM — this builds one stage only, and a \
                         multi-stage Dockerfile would silently build the last stage over the \
                         wrong base"
                    );
                    anyhow::ensure!(!rest.is_empty(), "Dockerfile:{line}: FROM names no image");
                    anyhow::ensure!(
                        !rest.contains(" AS ") && !rest.contains(" as "),
                        "Dockerfile:{line}: a named stage — this builds one stage only"
                    );
                    base = Some(rest.to_string());
                }
                // No steps before a base. After the `FROM` arm, the one instruction allowed
                // first.
                _ if base.is_none() => anyhow::bail!(
                    "Dockerfile:{line}: {instruction} before any FROM — a build has to start \
                     from a base"
                ),
                "RUN" => {
                    // An empty one would run `sh -c ""` and exit 0, a do-nothing step that
                    // usually means a broken edited continuation, worth reporting.
                    anyhow::ensure!(!rest.is_empty(), "Dockerfile:{line}: RUN names no command");
                    steps.push(Step::Run(rest.to_string()));
                }
                // One or more `ENV k=v` only. The legacy `ENV k v` is refused rather than
                // guessed at: `ENV JAVA_OPTS -Dfoo=bar` has an `=` in it, but no pair.
                "ENV" => {
                    anyhow::ensure!(!rest.is_empty(), "Dockerfile:{line}: ENV names no variable");
                    // Split on whitespace outside quotes: `ENV MESSAGE="hello world"` is one
                    // pair, not a pair plus a bare `world"`.
                    let mut pairs: Vec<String> = Vec::new();
                    let mut pair = String::new();
                    let mut quote: Option<char> = None;
                    for c in rest.chars() {
                        match quote {
                            Some(q) => {
                                quote = (c != q).then_some(q);
                                pair.push(c);
                            }
                            None if c == '"' || c == '\'' => {
                                quote = Some(c);
                                pair.push(c);
                            }
                            None if c.is_whitespace() => {
                                if !pair.is_empty() {
                                    pairs.push(std::mem::take(&mut pair));
                                }
                            }
                            None => pair.push(c),
                        }
                    }
                    if !pair.is_empty() {
                        pairs.push(pair);
                    }
                    for pair in pairs {
                        let (key, value) = pair.split_once('=').with_context(|| {
                            format!(
                                "Dockerfile:{line}: ENV with a bare word ({pair}) — this reads \
                                 `ENV k=v` only, not the legacy `ENV k v`"
                            )
                        })?;
                        // Strip one matching pair of surrounding quotes only:
                        // `ENV TZ="Asia/Seoul"` loses them, `ENV A=""x""` keeps the inner pair.
                        let value = value
                            .strip_prefix('"')
                            .and_then(|v| v.strip_suffix('"'))
                            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                            .unwrap_or(value);
                        steps.push(Step::env(key, value));
                    }
                }
                "WORKDIR" => {
                    anyhow::ensure!(
                        !rest.is_empty(),
                        "Dockerfile:{line}: WORKDIR names no directory"
                    );
                    steps.push(Step::Workdir(rest.to_string()));
                }
                other => anyhow::bail!(
                    "Dockerfile:{line}: {other} is not one of FROM, RUN, ENV and WORKDIR, \
                     which is all a declaration has"
                ),
            }
        }
        let base = base.context("this Dockerfile has no FROM, so there is no base to build on")?;
        Ok(Recipe::new(base).steps(steps))
    }

    /// One step, after everything declared so far.
    ///
    /// Takes anything convertible into a [`Step`], including a bare command:
    /// `.step("apk add jq")` is a `RUN`.
    pub fn step(mut self, step: impl Into<Step>) -> Self {
        self.steps.push(step.into());
        self
    }

    /// Append every step in `steps`, in order.
    pub fn steps(mut self, steps: impl IntoIterator<Item = impl Into<Step>>) -> Self {
        self.steps.extend(steps.into_iter().map(Into::into));
        self
    }
}

/// Refuse a version this virtx does not speak.
///
/// A missing version is refused too (by serde): it is not one of our declarations, and
/// assuming version 1 would invent its provenance.
fn known_version<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let found = u32::deserialize(d)?;
    if found != FORMAT_VERSION {
        return Err(serde::de::Error::custom(format!(
            "this image is written in format {found}, and this virtx reads {FORMAT_VERSION}"
        )));
    }
    Ok(found)
}

/// Refuse an empty base, which is not a base to build on.
fn some_base<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let base = String::deserialize(d)?;
    if base.is_empty() {
        return Err(serde::de::Error::custom(
            "this image names no base, and there is no build without one",
        ));
    }
    Ok(base)
}

/// One instruction of a build.
///
/// The declared form, not the wire form (where `RUN` becomes an argv prefixed by the
/// accumulated environment and `ENV` becomes nothing), so a build is named by the digest of
/// what it *declares*: identical declarations get the same image.
///
/// Serialized by name (`{"run": …}`, `{"env": {…}}`), not position, so a new variant cannot
/// change what a stored [`Recipe`] means.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// A command, run through `sh -c` with the environment accumulated so far.
    Run(String),

    /// A variable for every later [`Run`](Self::Run) and the built image's config.
    Env { key: String, value: String },

    /// Where later steps run, and the built image's working directory.
    Workdir(String),
}

impl Step {
    /// A command, run through `sh -c` with every earlier [`env`](Self::env).
    pub fn run(cmd: impl Into<String>) -> Self {
        Step::Run(cmd.into())
    }

    /// A variable for every later [`run`](Self::run) and the built image's config.
    pub fn env(key: impl Into<String>, value: impl Into<String>) -> Self {
        Step::Env {
            key: key.into(),
            value: value.into(),
        }
    }

    /// Where later steps run, and the built image's working directory.
    pub fn workdir(dir: impl Into<String>) -> Self {
        Step::Workdir(dir.into())
    }
}

/// A bare string is a `RUN`.
///
/// `RUN` is the only instruction that is a single string, so a lone command is unambiguous.
/// Other steps have no conversion; use [`Step::env`] and [`Step::workdir`].
impl From<&str> for Step {
    fn from(command: &str) -> Self {
        Step::run(command)
    }
}

impl From<String> for Step {
    fn from(command: String) -> Self {
        Step::Run(command)
    }
}

impl fmt::Display for Step {
    /// As the Dockerfile instruction, so build progress shows lines the caller recognises.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Run(command) => write!(f, "RUN {command}"),
            Step::Env { key, value } => write!(f, "ENV {key}={value}"),
            Step::Workdir(dir) => write!(f, "WORKDIR {dir}"),
        }
    }
}
