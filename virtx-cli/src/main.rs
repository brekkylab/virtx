//! `virtx`: a virtx server's images built and managed from a shell (`virtx_core::cli`), and
//! signing in to a server's OpenID Connect provider from a terminal, for the SDKs to use:
//!
//! ```text
//! virtx login [url]   sign in to the server at `url`: approve the code it shows, in a browser
//! virtx token         print an ID token, refreshed as needed; --json, with how long it
//!                     lasts, as the SDKs run it for a token
//! virtx whoami        who is signed in
//! virtx logout        sign out: the session ends
//! ```
//!
//! Signing in is RFC 8628's device flow against the provider the server at `url` names, as
//! the public client it names (`PROTOCOL.md`, Authentication) -- or those `--issuer`
//! (`VIRTX_OIDC_ISSUER`) and `--client-id` (`VIRTX_OIDC_CLIENT_ID`) name, the `issuer` and
//! `client_id` of the server's `oidc`. The token is the ID token, which the server checks
//! against the provider's keys. The other commands are of the provider last signed in to,
//! unless given one.
//!
//! Many processes may ask for a token at once -- an SDK runs `virtx token --json` in each --
//! and a provider that replaces the refresh token with each refresh takes one presented twice
//! for stolen. So `virtx token` holds a lock while it reads, refreshes and keeps, and gives
//! the ID token it last got for as long as that has more than [`FRESH_FOR`] left: one refresh
//! serves them all. Run with `VIRTX_TOKEN_REFUSED` set, as an SDK runs it once a server
//! refused the token it had, it refreshes whatever it has.

mod provider;
mod session;

use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};
use serde_json::json;

use crate::provider::{Asked, Provider, Tokens};
use crate::session::{Kept, Session, SignIn};

/// How long an ID token kept has to have left to be given again: more than the minute before
/// it runs out in which an SDK asks for another.
const FRESH_FOR: u64 = 120;

#[derive(Parser)]
#[command(
    name = "virtx",
    version,
    about = "Sign in to a virtx server's provider"
)]
struct Cli {
    /// The OpenID Connect provider: the `issuer` of the server's `oidc`.
    #[arg(long, global = true, env = "VIRTX_OIDC_ISSUER")]
    issuer: Option<String>,
    /// The client to sign in as: the `client_id` of the server's `oidc`.
    #[arg(long, global = true, env = "VIRTX_OIDC_CLIENT_ID")]
    client_id: Option<String>,
    /// The client's secret, for a provider that gives its public clients one.
    #[arg(long, global = true, env = "VIRTX_OIDC_CLIENT_SECRET")]
    client_secret: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in, approving the code shown.
    Login {
        /// The server to sign in to, which says who its provider is.
        url: Option<String>,
        /// What is asked of the provider: `offline_access` for a session that lasts.
        #[arg(long, default_value = "openid email profile offline_access")]
        scope: String,
    },
    /// Print an ID token, refreshed.
    Token {
        /// Print `{"token": ..., "expires_in": <seconds>}`, as the SDKs read it.
        #[arg(long)]
        json: bool,
    },
    /// Say who is signed in.
    Whoami,
    /// Sign out, ending the session.
    Logout,
}

/// The commands of signing in, as the first argument names them; any other is
/// `virtx_core::cli`'s.
const SIGNING_IN: [&str; 4] = ["login", "token", "whoami", "logout"];

fn main() -> std::process::ExitCode {
    let signing_in = std::env::args()
        .nth(1)
        .is_some_and(|first| SIGNING_IN.contains(&first.as_str()));
    if !signing_in {
        return std::process::ExitCode::from(virtx_core::cli::main(std::env::args_os()));
    }
    let run = || {
        let cli = Cli::parse();
        let dir = session::dir()?;
        let server = match &cli.command {
            Command::Login { url, .. } => url.as_deref(),
            _ => None,
        };
        let sign_in = sign_in(&cli, server, &dir)?;
        let refused = std::env::var_os("VIRTX_TOKEN_REFUSED").is_some();
        match cli.command {
            Command::Login { scope, .. } => login(&dir, &sign_in, &scope, true, announce),
            Command::Token { json } => {
                let kept = token(&dir, &sign_in, refused, true)?;
                match json {
                    true => {
                        let expires_in = kept.expires_at.saturating_sub(now());
                        println!(
                            "{}",
                            json!({ "token": kept.token, "expires_in": expires_in })
                        );
                    }
                    false => println!("{}", kept.token),
                }
                Ok(())
            }
            Command::Whoami => {
                let kept = token(&dir, &sign_in, false, true)?;
                println!("{}", who(&kept.token));
                Ok(())
            }
            Command::Logout => logout(&dir, &sign_in, true),
        }
    };
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("virtx: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The provider `cli` names, or else the one `server` says to sign in to, or else the one
/// last signed in to.
fn sign_in(cli: &Cli, server: Option<&str>, dir: &Path) -> anyhow::Result<SignIn> {
    let (issuer, client_id) = match (&cli.issuer, &cli.client_id, server) {
        (Some(issuer), Some(client_id), _) => (issuer.clone(), client_id.clone()),
        (None, None, Some(server)) => provider::advertised(server)?
            .with_context(|| format!("{server} names no provider to sign in to"))?,
        (None, None, None) => {
            return session::signed_in(dir)
                .context("not signed in; run virtx login <the server's URL>");
        }
        _ => bail!("a provider is named by both --issuer and --client-id"),
    };
    Ok(SignIn {
        issuer,
        client_id,
        client_secret: cli.client_secret.clone(),
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn discover(sign_in: &SignIn) -> anyhow::Result<Provider> {
    let secret = sign_in.client_secret.as_deref();
    Provider::discover(&sign_in.issuer, &sign_in.client_id, secret)
}

/// Whom `id_token` names, for a person to read.
fn who(id_token: &str) -> String {
    let claims = provider::claims(id_token).unwrap_or_default();
    let named = claims["email"].as_str().or(claims["sub"].as_str());
    named.unwrap_or("?").to_owned()
}

/// Keep what the provider issued as the session's, and give its ID token.
fn keep(session: &Session, tokens: Tokens) -> anyhow::Result<Kept> {
    let token = tokens
        .id_token
        .context("the provider issued no ID token: the scope asked has no `openid`")?;
    let expires_at = provider::claims(&token)?["exp"]
        .as_u64()
        .context("the ID token says not when it runs out")?;
    if let Some(refresh) = &tokens.refresh_token {
        session.keep(refresh)?;
    }
    let kept = Kept { token, expires_at };
    session.keep_id(&kept)?;
    Ok(kept)
}

/// Show a person the `page` to approve `code` on, opened in their browser if there is one.
fn announce(code: &str, page: &str) {
    eprintln!("To sign in, open this page and approve the code {code}:");
    eprintln!();
    eprintln!("    {page}");
    eprintln!();
    open(page);
}

/// Sign in to `sign_in`'s provider asking for `scope`, the code to approve shown by `announce`.
fn login(
    dir: &Path,
    sign_in: &SignIn,
    scope: &str,
    keychain: bool,
    announce: fn(&str, &str),
) -> anyhow::Result<()> {
    let provider = discover(sign_in)?;
    let codes = provider.device(scope)?;
    let page = codes
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&codes.verification_uri);
    announce(&codes.user_code, page);

    let mut interval = Duration::from_secs(codes.interval.max(1));
    let deadline = Instant::now() + Duration::from_secs(codes.expires_in);
    while Instant::now() < deadline {
        sleep(interval);
        match provider.ask(&codes.device_code)? {
            Asked::Pending => {}
            Asked::SlowDown => interval += Duration::from_secs(5),
            Asked::Issued(tokens) => {
                let session = Session::new(dir, &sign_in.issuer, keychain);
                let _lock = session.lock()?;
                // Another session's tokens are not this one's to refresh with.
                session.forget()?;
                let kept = keep(&session, tokens)?;
                session::keep_sign_in(dir, sign_in)?;
                eprintln!("Signed in as {}.", who(&kept.token));
                return Ok(());
            }
        }
    }
    bail!("the code expired; run virtx login again")
}

/// An ID token: the one kept while it has more than [`FRESH_FOR`] left and the server has not
/// `refused` it, or else one refreshed from the session kept. All under the lock.
fn token(dir: &Path, sign_in: &SignIn, refused: bool, keychain: bool) -> anyhow::Result<Kept> {
    let session = Session::new(dir, &sign_in.issuer, keychain);
    let _lock = session.lock()?;
    if !refused
        && let Some(kept) = session.kept_id()
        && kept.expires_at.saturating_sub(now()) > FRESH_FOR
    {
        return Ok(kept);
    }
    let refresh = session.kept()?.context("not signed in; run virtx login")?;
    match discover(sign_in)?.refresh(&refresh)? {
        Some(tokens) => keep(&session, tokens),
        None => {
            // Kept as it is: under the lock no other run of this replaced it, so the session
            // has ended, and `virtx login` puts a new one in its place.
            let _ = session.forget_id();
            bail!("the session has ended; run virtx login")
        }
    }
}

fn logout(dir: &Path, sign_in: &SignIn, keychain: bool) -> anyhow::Result<()> {
    let session = Session::new(dir, &sign_in.issuer, keychain);
    let _lock = session.lock()?;
    if let Some(refresh) = session.kept()?
        && let Ok(provider) = discover(sign_in)
    {
        provider.revoke(&refresh);
    }
    session.forget()?;
    session.forget_id()?;
    eprintln!("Signed out.");
    Ok(())
}

/// Open `url` in the person's browser, if there is one to open it in: the page is printed
/// above for when there is not.
fn open(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    //! Against the fixture: its server, and the provider that names to sign in to, which
    //! approves a device at the second ask and replaces the refresh token with each refresh.

    use super::*;

    /// The fixture's server, and the provider it says to sign in to.
    fn fixture() -> (String, SignIn) {
        let url = std::env::var("VIRTX_TEST_URL")
            .expect("VIRTX_TEST_URL is not set: run these as `cargo xtask test -p virtx-cli`");
        let (issuer, client_id) = provider::advertised(&url).unwrap().unwrap();
        let sign_in = SignIn {
            issuer,
            client_id,
            client_secret: None,
        };
        (url, sign_in)
    }

    #[test]
    fn signing_in_keeps_a_session_its_tokens_are_refreshed_from_until_signed_out() {
        let (url, sign_in) = fixture();
        let dir = std::env::temp_dir().join(format!("virtx-cli-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        login(&dir, &sign_in, "openid", false, |_, _| {}).unwrap();
        assert_eq!(session::signed_in(&dir).unwrap().issuer, sign_in.issuer);

        // The one signing in issued has a minute left: refreshed, and the one after it kept.
        let first = token(&dir, &sign_in, false, false).unwrap();
        assert_eq!(who(&first.token), "test@example.com");
        assert!(first.expires_at - now() > FRESH_FOR);
        let again = token(&dir, &sign_in, false, false).unwrap();
        assert_eq!(again.token, first.token);
        // The server takes it.
        let listed = reqwest::blocking::Client::new()
            .get(format!("{url}volumes/"))
            .bearer_auth(&first.token)
            .header(
                "virtx-project",
                std::env::var("VIRTX_TEST_PROJECT").unwrap(),
            )
            .send()
            .unwrap();
        assert_eq!(listed.status(), 200);
        // Refused by a server, it is refreshed whatever it has left.
        let renewed = token(&dir, &sign_in, true, false).unwrap();
        assert_ne!(renewed.token, first.token);

        // Signing out ends the session at the provider too.
        let session = Session::new(&dir, &sign_in.issuer, false);
        let refresh = session.kept().unwrap().unwrap();
        logout(&dir, &sign_in, false).unwrap();
        assert!(
            discover(&sign_in)
                .unwrap()
                .refresh(&refresh)
                .unwrap()
                .is_none()
        );
        let ended = token(&dir, &sign_in, false, false).err().unwrap();
        assert!(ended.to_string().contains("not signed in"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_the_provider_has_ended_says_to_sign_in_again() {
        let (_, sign_in) = fixture();
        let dir = std::env::temp_dir().join(format!("virtx-cli-ended-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let session = Session::new(&dir, &sign_in.issuer, false);
        session.keep("r-stale").unwrap();
        let ended = token(&dir, &sign_in, false, false).err().unwrap();
        assert!(ended.to_string().contains("the session has ended"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
