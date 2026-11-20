use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use clap::{Args, Subcommand};
use serde_json::json;

use super::provider::{self, Asked, Provider, Tokens};
use super::session::{self, Kept, Session, SignIn, SignedIn};
use crate::cli::cloud::{self, CloudArgs, CloudCommand, DOMAIN};

/// How long an ID token kept has to have left to be given again: more than the minute before
/// it runs out in which an SDK asks for another.
const FRESH_FOR: u64 = 120;

/// Which provider and client a command of signing in is of, where not the one a server names
/// or the one last signed in to. Global, so they go before or after the command.
#[derive(Debug, Args)]
#[command(next_help_heading = "Signing in")]
pub(in crate::cli) struct ProviderArgs {
    /// The OpenID Connect provider: the `issuer` of the server's `oidc`.
    #[arg(long, global = true, env = "VIRTX_OIDC_ISSUER")]
    pub(in crate::cli) issuer: Option<String>,
    /// The client to sign in as: the `client_id` of the server's `oidc`.
    #[arg(long, global = true, env = "VIRTX_OIDC_CLIENT_ID")]
    pub(in crate::cli) client_id: Option<String>,
    /// The client's secret, for a provider that gives its public clients one.
    #[arg(long, global = true, env = "VIRTX_OIDC_CLIENT_SECRET")]
    pub(in crate::cli) client_secret: Option<String>,
}

#[derive(Debug, PartialEq, Subcommand)]
pub(in crate::cli) enum SignInCommand {
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

/// Run `command`, and answer its exit code: of `provider` where one is named -- by its issuer
/// and client, or the server `login` is given -- of virtx cloud where `cloud` names its domain,
/// and otherwise of what was signed in to last, virtx cloud where nothing was. A provider's HTTP
/// blocks, so it runs on no runtime's thread; virtx cloud's runs on one of its own.
pub(in crate::cli) fn run(
    provider: &ProviderArgs,
    cloud: &CloudArgs,
    command: SignInCommand,
) -> u8 {
    let run = || {
        let dir = session::dir()?;
        let server = match &command {
            SignInCommand::Login { url, .. } => url.as_deref(),
            _ => None,
        };
        let sign_in = match sign_in(provider, server, cloud.domain.is_some(), &dir)? {
            Of::Provider(sign_in) => sign_in,
            Of::Cloud(kept) => {
                let domain = cloud
                    .domain
                    .clone()
                    .or(kept)
                    .unwrap_or_else(|| DOMAIN.into());
                return of_cloud(&domain, command);
            }
        };
        let refused = std::env::var_os("VIRTX_TOKEN_REFUSED").is_some();
        match command {
            SignInCommand::Login { scope, .. } => login(&dir, &sign_in, &scope, open),
            SignInCommand::Token { json } => {
                let kept = token(&dir, &sign_in, refused)?;
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
            SignInCommand::Whoami => {
                let kept = token(&dir, &sign_in, false)?;
                println!("{}", who(&kept.token));
                Ok(())
            }
            SignInCommand::Logout => logout(&dir, &sign_in),
        }
    };
    match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("virtx: {e:#}");
            1
        }
    }
}

/// What a command of signing in is of: a provider, or virtx cloud, at the domain last signed in
/// to if it was.
enum Of {
    Provider(SignIn),
    Cloud(Option<String>),
}

/// The provider `provider` names, or else the one `server` says to sign in to; or else virtx
/// cloud, where `cloud` names it; or else what was signed in to last, and virtx cloud where
/// nothing was.
fn sign_in(
    provider: &ProviderArgs,
    server: Option<&str>,
    cloud: bool,
    dir: &Path,
) -> anyhow::Result<Of> {
    let (issuer, client_id) = match (&provider.issuer, &provider.client_id, server) {
        (Some(issuer), Some(client_id), _) => (issuer.clone(), client_id.clone()),
        (None, None, Some(server)) => provider::advertised(server)?
            .with_context(|| format!("{server} names no provider to sign in to"))?,
        (None, None, None) if cloud => return Ok(Of::Cloud(None)),
        (None, None, None) => {
            return Ok(match session::signed_in(dir) {
                Some(SignedIn::Provider(sign_in)) => Of::Provider(sign_in),
                Some(SignedIn::Cloud { domain }) => Of::Cloud(Some(domain)),
                None => Of::Cloud(None),
            });
        }
        _ => bail!("a provider is named by both --issuer and --client-id"),
    };
    Ok(Of::Provider(SignIn {
        issuer,
        client_id,
        client_secret: provider.client_secret.clone(),
    }))
}

/// `command` of virtx cloud at `domain`, on a runtime of its own.
fn of_cloud(domain: &str, command: SignInCommand) -> anyhow::Result<()> {
    let command = match command {
        SignInCommand::Login { .. } => CloudCommand::Login,
        SignInCommand::Token { json } => CloudCommand::Token { json },
        SignInCommand::Whoami => CloudCommand::Whoami,
        SignInCommand::Logout => CloudCommand::Logout,
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(cloud::run(domain, command))
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

/// Show a person the `page` to approve `code` on, and `open` the URL it reads as where that is
/// a web page: `http` or `https`. The provider answers the page, and what opens one takes a
/// path, or another scheme's URL, as readily.
fn announce(code: &str, page: &str, open: impl FnOnce(&str)) {
    eprintln!("To sign in, open this page and approve the code {code}:");
    eprintln!();
    eprintln!("    {page}");
    eprintln!();
    match reqwest::Url::parse(page)
        .ok()
        .filter(|page| matches!(page.scheme(), "http" | "https"))
    {
        Some(page) => open(page.as_str()),
        None => {
            eprintln!("virtx did not open it: it is not an http or https page.");
            eprintln!();
        }
    }
}

/// Sign in to `sign_in`'s provider asking for `scope`, the page to approve the code on opened
/// by `open`.
fn login(dir: &Path, sign_in: &SignIn, scope: &str, open: impl FnOnce(&str)) -> anyhow::Result<()> {
    let provider = discover(sign_in)?;
    let codes = provider.device(scope)?;
    let page = codes
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&codes.verification_uri);
    announce(&codes.user_code, page, open);

    let mut interval = Duration::from_secs(codes.interval.max(1));
    let deadline = Instant::now() + Duration::from_secs(codes.expires_in);
    while Instant::now() < deadline {
        sleep(interval);
        match provider.ask(&codes.device_code)? {
            Asked::Pending => {}
            Asked::SlowDown => interval += Duration::from_secs(5),
            Asked::Issued(tokens) => {
                let session = Session::new(dir, sign_in);
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
fn token(dir: &Path, sign_in: &SignIn, refused: bool) -> anyhow::Result<Kept> {
    let session = Session::new(dir, sign_in);
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

fn logout(dir: &Path, sign_in: &SignIn) -> anyhow::Result<()> {
    let session = Session::new(dir, sign_in);
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
    //! Against the fixture where a provider is asked: its server, and the provider that names
    //! to sign in to, which approves a device at the second ask and replaces the refresh token
    //! with each refresh.

    use super::*;

    /// The fixture's server, and the provider it says to sign in to.
    fn fixture() -> (String, SignIn) {
        let url = std::env::var("VIRTX_TEST_URL")
            .expect("VIRTX_TEST_URL is not set: run these under `cargo xtask fixture`");
        let (issuer, client_id) = provider::advertised(&url).unwrap().unwrap();
        let sign_in = SignIn {
            issuer,
            client_id,
            client_secret: None,
        };
        (url, sign_in)
    }

    #[test]
    #[ignore = "against the fixture's provider, as `cargo xtask test` runs it"]
    fn signing_in_keeps_a_session_its_tokens_are_refreshed_from_until_signed_out() {
        let (url, sign_in) = fixture();
        let dir = std::env::temp_dir().join(format!("virtx-cli-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut opened = None;
        let open = |page: &str| opened = Some(page.to_owned());
        login(&dir, &sign_in, "openid", open).unwrap();
        // The page the provider answered, a web page.
        assert_eq!(opened, Some(format!("{}/approve", sign_in.issuer)));
        let Some(SignedIn::Provider(signed_in)) = session::signed_in(&dir) else {
            panic!("the provider is not kept as signed in to");
        };
        assert_eq!(signed_in.issuer, sign_in.issuer);

        // The one signing in issued has a minute left: refreshed, and the one after it kept.
        let first = token(&dir, &sign_in, false).unwrap();
        assert_eq!(who(&first.token), "test@example.com");
        assert!(first.expires_at - now() > FRESH_FOR);
        let again = token(&dir, &sign_in, false).unwrap();
        assert_eq!(again.token, first.token);
        // Another client of the provider has a session of its own: none, not signed in as it.
        let another = SignIn {
            client_id: "another".into(),
            ..sign_in.clone()
        };
        let unsigned = token(&dir, &another, false).err().unwrap();
        assert!(unsigned.to_string().contains("not signed in"));
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
        let renewed = token(&dir, &sign_in, true).unwrap();
        assert_ne!(renewed.token, first.token);

        // Signing out ends the session at the provider too.
        let session = Session::new(&dir, &sign_in);
        let refresh = session.kept().unwrap().unwrap();
        logout(&dir, &sign_in).unwrap();
        assert!(
            discover(&sign_in)
                .unwrap()
                .refresh(&refresh)
                .unwrap()
                .is_none()
        );
        let ended = token(&dir, &sign_in, false).err().unwrap();
        assert!(ended.to_string().contains("not signed in"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "against the fixture's provider, as `cargo xtask test` runs it"]
    fn runs_at_once_refresh_a_session_once_and_share_what_it_gave() {
        let (_, sign_in) = fixture();
        let dir = std::env::temp_dir().join(format!("virtx-cli-at-once-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        login(&dir, &sign_in, "openid", |_| {}).unwrap();

        // The one signing in issued has a minute left, so each would refresh it; the provider takes
        // a refresh token once.
        let tokens: Vec<_> = std::thread::scope(|scope| {
            let runs: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| token(&dir, &sign_in, false)))
                .collect();
            let ran = runs.into_iter().map(|run| run.join().unwrap());
            ran.map(|kept| kept.unwrap().token).collect()
        });
        assert!(tokens.iter().all(|token| *token == tokens[0]), "{tokens:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_page_to_approve_on_is_opened_only_where_it_is_a_web_page() {
        let device = "https://idp.example/device?user_code=ABCD";
        let local = "http://127.0.0.1:8080/approve";
        for (page, opens) in [
            (device, Some(device)),
            (local, Some(local)),
            // As the URL it was read as.
            (" HTTPS://idp.example/device?user_code=ABCD", Some(device)),
            ("file:///System/Applications/Calculator.app", None),
            ("/System/Applications/Calculator.app", None),
            (r"C:\Windows\System32\calc.exe", None),
            (r"\\files.example\share\setup.exe", None),
            ("ssh://idp.example", None),
            ("-a Calculator", None),
        ] {
            let mut opened = None;
            announce("ABCD", page, |page| opened = Some(page.to_owned()));
            assert_eq!(opened.as_deref(), opens, "{page}");
        }
    }

    #[test]
    #[ignore = "against the fixture's provider, as `cargo xtask test` runs it"]
    fn a_session_the_provider_has_ended_says_to_sign_in_again() {
        let (_, sign_in) = fixture();
        let dir = std::env::temp_dir().join(format!("virtx-cli-ended-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let session = Session::new(&dir, &sign_in);
        session.keep("r-stale").unwrap();
        let ended = token(&dir, &sign_in, false).err().unwrap();
        assert!(ended.to_string().contains("the session has ended"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What `argv` is of, with `kept` as what was signed in to last: a provider's issuer, or
    /// virtx cloud's domain if it was kept, `None` for virtx cloud's where none was.
    fn of(argv: &[&str], kept: Option<&str>) -> Result<String, Option<String>> {
        let dir = std::env::temp_dir().join(format!(
            "virtx-cli-of-{}-{}",
            std::process::id(),
            argv.join("-").replace('/', "_")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(kept) = kept {
            std::fs::write(dir.join("signed-in.json"), kept).unwrap();
        }
        let Ok(crate::cli::command::Asked::SignIn(provider, cloud, command)) =
            crate::cli::command::parse(argv.iter().copied())
        else {
            panic!("{argv:?} is no command of signing in");
        };
        let server = match &command {
            SignInCommand::Login { url, .. } => url.as_deref(),
            _ => None,
        };
        let of = sign_in(&provider, server, cloud.domain.is_some(), &dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        match of {
            Of::Provider(sign_in) => Ok(sign_in.issuer),
            Of::Cloud(domain) => Err(domain),
        }
    }

    #[test]
    fn a_command_is_of_what_it_names_or_else_of_what_was_signed_in_to_last_or_else_virtx_cloud() {
        let provider = r#"{"issuer": "https://idp.invalid", "client_id": "virtx"}"#;
        // As `cloud.rs` keeps virtx cloud's.
        let cloud = r#"{"domain": "dev.virtx.sh"}"#;
        let named = [
            "virtx",
            "token",
            "--issuer",
            "https://other.invalid",
            "--client-id",
            "c",
        ];
        assert_eq!(of(&named, Some(cloud)), Ok("https://other.invalid".into()));
        let domain = ["virtx", "whoami", "--domain", "x.invalid"];
        assert_eq!(of(&domain, Some(provider)), Err(None));
        for command in ["login", "token", "whoami", "logout"] {
            let argv = ["virtx", command];
            assert_eq!(of(&argv, Some(provider)), Ok("https://idp.invalid".into()));
            assert_eq!(of(&argv, Some(cloud)), Err(Some("dev.virtx.sh".into())));
            assert_eq!(of(&argv, None), Err(None));
        }
    }
}
