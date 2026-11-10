//! The token each request carries, as the client's [`Auth`] gives it: the one given, or the one
//! a command prints, kept until it runs out or the server refuses it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock};
use std::time::{Duration, Instant};

use crate::Auth;

/// How long before a command's token runs out it is run again, so that no request goes with
/// one that runs out on its way.
const MARGIN: Duration = Duration::from_secs(60);

/// How long a command has to print a token: one waiting on a person -- a keychain's prompt no
/// one sees, from a process with no terminal -- is killed, and the request fails saying so.
const PRINTS_WITHIN: Duration = if cfg!(test) {
    Duration::from_secs(2)
} else {
    Duration::from_secs(60)
};

/// The client's [`Auth`], and what its command last printed.
pub(super) struct Credentials {
    auth: RwLock<Auth>,
    /// Moves on with each new auth and each refusal: a token issued before is not sent again.
    generation: AtomicU64,
    /// Moves on with each refusal alone.
    refusals: AtomicU64,
    /// Held while the command runs, so that requests sent together run it once.
    issued: tokio::sync::Mutex<Option<Issued>>,
}

struct Issued {
    token: String,
    generation: u64,
    refusals: u64,
    /// When it is run again; never, for a token printed bare, until the server refuses it.
    until: Option<Instant>,
}

impl Credentials {
    pub(super) fn new(auth: Auth) -> Self {
        Credentials {
            auth: RwLock::new(auth),
            generation: AtomicU64::new(0),
            refusals: AtomicU64::new(0),
            issued: tokio::sync::Mutex::new(None),
        }
    }

    /// Send `auth` from now on, a command's token printed before forgotten.
    pub(super) fn set(&self, auth: Auth) {
        *self.auth.write().unwrap_or_else(PoisonError::into_inner) = auth;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Whether a token refused is printed anew: an [`Auth::Command`]'s.
    pub(super) fn renews(&self) -> bool {
        matches!(
            *self.auth.read().unwrap_or_else(PoisonError::into_inner),
            Auth::Command { .. }
        )
    }

    /// The server refused the token sent: a command prints a new one for the next request.
    pub(super) fn refused(&self) {
        self.refusals.fetch_add(1, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// The token to send now, if any: an [`Auth::Command`]'s is the one it printed last while
    /// that holds, or else what it prints when run again.
    pub(super) async fn token(&self) -> Result<Option<String>, String> {
        let auth = self
            .auth
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let (program, args) = match auth {
            Auth::None => return Ok(None),
            Auth::Bearer { token } => return Ok(Some(token)),
            Auth::Command { program, args } => (program, args),
        };
        let mut issued = self.issued.lock().await;
        let generation = self.generation.load(Ordering::SeqCst);
        let refusals = self.refusals.load(Ordering::SeqCst);
        if let Some(issued) = issued.as_ref()
            && issued.generation == generation
            && issued.until.is_none_or(|until| Instant::now() < until)
        {
            return Ok(Some(issued.token.clone()));
        }
        // Refused since it last ran, it is told so: a command that keeps its tokens, such as
        // `virtx token`, gets a new one rather than the one refused.
        let refused = issued
            .as_ref()
            .is_some_and(|issued| issued.refusals != refusals);
        let (token, expires_in) = run(&program, &args, refused).await?;
        *issued = Some(Issued {
            token: token.clone(),
            generation,
            refusals,
            until: expires_in.map(|lasts| Instant::now() + lasts.saturating_sub(MARGIN)),
        });
        Ok(Some(token))
    }
}

/// What `program` with `args` prints: a token alone, or `{"token": ..., "expires_in": <seconds>}`.
/// Run with `VIRTX_TOKEN_REFUSED=1` in its environment once the server `refused` its last.
async fn run(
    program: &str,
    args: &[String],
    refused: bool,
) -> Result<(String, Option<Duration>), String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    if refused {
        command.env("VIRTX_TOKEN_REFUSED", "1");
    }
    let output = tokio::time::timeout(PRINTS_WITHIN, command.output())
        .await
        .map_err(|_| {
            format!(
                "{program} printed no token in {}s: waiting on something, such as the \
                 OS asking whether it may read the keychain",
                PRINTS_WITHIN.as_secs()
            )
        })?
        .map_err(|e| format!("running {program} for a token: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{program} printed no token ({}): {}",
            output.status,
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();
    let (token, expires_in) = if stdout.starts_with('{') {
        let printed: serde_json::Value = serde_json::from_str(stdout)
            .map_err(|e| format!("{program} printed what is not a token: {e}"))?;
        let token = printed["token"]
            .as_str()
            .ok_or_else(|| format!("{program} printed no \"token\""))?;
        let expires_in = printed["expires_in"].as_u64().map(Duration::from_secs);
        (token.to_owned(), expires_in)
    } else {
        (stdout.to_owned(), None)
    };
    if token.is_empty() {
        return Err(format!("{program} printed no token"));
    }
    Ok((token, expires_in))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Credentials {
        Credentials::new(Auth::Command {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
        })
    }

    /// A script printing how many times it has run, as a token lasting `expires_in` seconds.
    fn counting(name: &str, expires_in: u64) -> Credentials {
        let runs = std::env::temp_dir().join(format!("virtx-token-{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&runs);
        let runs = runs.display();
        sh(&format!(
            r#"echo x >> {runs}; printf '{{"token": "%s", "expires_in": {expires_in}}}' $(wc -l < {runs})"#
        ))
    }

    #[tokio::test]
    async fn a_token_printed_bare_is_kept_until_the_server_refuses_it() {
        let credentials =
            sh(r#"if [ -n "$VIRTX_TOKEN_REFUSED" ]; then echo new; else echo old; fi"#);
        assert_eq!(credentials.token().await.unwrap().as_deref(), Some("old"));
        assert_eq!(credentials.token().await.unwrap().as_deref(), Some("old"));
        credentials.refused();
        assert_eq!(credentials.token().await.unwrap().as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn a_token_is_printed_again_within_a_minute_of_running_out() {
        let lasting = counting("lasting", 3600);
        assert_eq!(lasting.token().await.unwrap().as_deref(), Some("1"));
        assert_eq!(lasting.token().await.unwrap().as_deref(), Some("1"));

        let ending = counting("ending", 30);
        assert_eq!(ending.token().await.unwrap().as_deref(), Some("1"));
        assert_eq!(ending.token().await.unwrap().as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn a_command_that_prints_no_token_fails_the_request_with_what_it_said() {
        let failed = sh("echo not signed in >&2; exit 1").token().await;
        assert!(failed.unwrap_err().contains("not signed in"));
        let silent = sh("true").token().await;
        assert!(silent.unwrap_err().contains("printed no token"));
    }
}
