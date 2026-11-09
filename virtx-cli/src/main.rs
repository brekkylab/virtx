//! `virtx`: a virtx server's images built and managed from a shell (`virtx_core::cli`), and
//! signing in to virtx cloud from a terminal, for the SDKs to use -- these:
//!
//! ```text
//! virtx login     sign in: approve the code it shows in the console, in a browser
//! virtx token     print an access token, refreshed as needed (VIRTX_API_KEY takes it);
//!                 --json, with how long it lasts, as the SDKs run it for a token
//! virtx whoami    who is signed in
//! virtx logout    sign out: the session ends
//! ```
//!
//! Signing in is RFC 8628's device flow against the control plane at `https://api.<domain>`,
//! `<domain>` being `--domain`, `VIRTX_DOMAIN`, or [`DOMAIN`]. The session's refresh token is
//! kept in the OS's keychain, under the domain; where the OS has none, in
//! `~/.config/virtx/<domain>.token`, readable by its owner alone. An access token lasts 15
//! minutes.
//!
//! Many processes may ask for a token at once -- an SDK runs `virtx token --json` in each, as
//! an agent's workers start together -- and each refresh replaces the refresh token: two at
//! once would present one already replaced, which the control plane takes for a stolen token
//! and ends the session over. So `virtx token` holds `~/.config/virtx/<domain>.lock` while it
//! reads, refreshes and keeps, and gives the access token it last got
//! (`~/.config/virtx/<domain>.access`, its owner's alone) for as long as it has more than
//! [`FRESH_FOR`] left: one refresh serves them all. Run with `VIRTX_TOKEN_REFUSED` set, as an
//! SDK runs it once a server refused the token it had, it refreshes whatever it has.
//!
//! With an access token, a client names its organization as well -- `VIRTX_ORG`, its UUID --
//! and its cell's endpoint, `VIRTX_URL`: unlike an API key, a person's token names neither.

use std::fs::File;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Where virtx cloud is, by default, as the SDKs have it.
const DOMAIN: &str = "virtx.sh";
const KEYCHAIN: &str = "virtx";
/// How long an access token kept has to have left to be given again: more than the minute
/// before it runs out in which an SDK asks for another.
const FRESH_FOR: Duration = Duration::from_secs(120);

#[derive(Parser)]
#[command(name = "virtx", version, about = "virtx cloud from a terminal")]
struct Cli {
    /// Where virtx cloud is: its control plane is api.<domain>.
    #[arg(long, global = true, env = "VIRTX_DOMAIN", default_value = DOMAIN)]
    domain: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in, approving the code shown in the console.
    Login,
    /// Print an access token, refreshed.
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

#[derive(Deserialize)]
struct Codes {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize, Serialize)]
struct Tokens {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// An access token kept between runs of `virtx token`.
#[derive(Deserialize, Serialize)]
struct Access {
    token: String,
    /// Seconds since the epoch.
    expires_at: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

struct Cloud {
    api: String,
    domain: String,
    http: reqwest::blocking::Client,
}

/// The commands of virtx cloud's, as the first argument names them; any other is
/// `virtx_core::cli`'s.
const CLOUD: [&str; 4] = ["login", "token", "whoami", "logout"];

fn main() -> std::process::ExitCode {
    let cloud = std::env::args().nth(1).is_some_and(|first| CLOUD.contains(&first.as_str()));
    if !cloud {
        return std::process::ExitCode::from(virtx_core::cli::main(std::env::args_os()));
    }
    match cloud_main() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("virtx: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn cloud_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cloud = Cloud {
        api: format!("https://api.{}", cli.domain),
        domain: cli.domain,
        http: reqwest::blocking::Client::builder()
            .user_agent(concat!("virtx-cli/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()?,
    };
    match cli.command {
        Command::Login => cloud.login(),
        Command::Token { json } => {
            let tokens = cloud.token()?;
            if json {
                let printed = json!({
                    "token": tokens.access_token,
                    "expires_in": tokens.expires_in,
                });
                println!("{printed}");
            } else {
                println!("{}", tokens.access_token);
            }
            Ok(())
        }
        Command::Whoami => {
            let me = cloud.me(&cloud.token()?.access_token)?;
            println!("{}", me["user"]["email"].as_str().unwrap_or("?"));
            Ok(())
        }
        Command::Logout => cloud.logout(),
    }
}

impl Cloud {
    fn login(&self) -> anyhow::Result<()> {
        let codes: Codes = self
            .http
            .post(format!("{}/auth/device", self.api))
            .json(&json!({}))
            .send()?
            .error_for_status()?
            .json()?;
        eprintln!(
            "To sign in, open this page and approve the code {}:",
            codes.user_code
        );
        eprintln!();
        eprintln!("    {}", codes.verification_uri_complete);
        eprintln!();
        open(&codes.verification_uri_complete);

        let mut interval = Duration::from_secs(codes.interval.max(1));
        let deadline = Instant::now() + Duration::from_secs(codes.expires_in);
        while Instant::now() < deadline {
            sleep(interval);
            let answer = self
                .http
                .post(format!("{}/auth/device/token", self.api))
                .json(&json!({ "device_code": codes.device_code }))
                .send()?;
            if answer.status().is_success() {
                let tokens: Tokens = answer.json()?;
                let refresh = tokens
                    .refresh_token
                    .as_deref()
                    .context("no refresh token answered")?;
                let _lock = self.lock()?;
                self.keep(refresh)?;
                self.keep_access(&tokens)?;
                let me = self.me(&tokens.access_token)?;
                eprintln!(
                    "Signed in as {}.",
                    me["user"]["email"].as_str().unwrap_or("?")
                );
                return Ok(());
            }
            let error: Value = answer.json().unwrap_or_default();
            match error["error"].as_str() {
                Some("authorization_pending") => {}
                Some("slow_down") => interval += Duration::from_secs(5),
                Some("access_denied") => bail!("the code was denied"),
                Some("expired_token") => bail!("the code expired; run virtx login again"),
                _ => bail!("signing in failed: {error}"),
            }
        }
        bail!("the code expired; run virtx login again")
    }

    /// An access token: the one kept while it has more than [`FRESH_FOR`] left, or else one
    /// refreshed from the session kept, the refresh token replaced. All under the lock.
    fn token(&self) -> anyhow::Result<Tokens> {
        let _lock = self.lock()?;
        let refused = std::env::var_os("VIRTX_TOKEN_REFUSED").is_some();
        if !refused && let Some(access) = self.kept_access() {
            let left = access.expires_at.saturating_sub(now());
            if left > FRESH_FOR.as_secs() {
                return Ok(Tokens {
                    access_token: access.token,
                    refresh_token: None,
                    expires_in: Some(left),
                });
            }
        }
        let refresh = self.kept()?.context("not signed in; run virtx login")?;
        let answer = self
            .http
            .post(format!("{}/auth/token", self.api))
            .json(&json!({ "refresh_token": refresh }))
            .send()?;
        if answer.status() == reqwest::StatusCode::UNAUTHORIZED {
            // Kept as it is: under the lock no other run of this replaced it, so the session
            // has ended, and `virtx login` puts a new one in its place.
            let _ = self.forget_access();
            bail!("the session has ended; run virtx login");
        }
        let tokens: Tokens = answer.error_for_status()?.json()?;
        if let Some(refresh) = &tokens.refresh_token {
            self.keep(refresh)?;
        }
        self.keep_access(&tokens)?;
        Ok(tokens)
    }

    fn me(&self, access: &str) -> anyhow::Result<Value> {
        Ok(self
            .http
            .get(format!("{}/v1/me", self.api))
            .bearer_auth(access)
            .send()?
            .error_for_status()?
            .json()?)
    }

    fn logout(&self) -> anyhow::Result<()> {
        if let Some(refresh) = self.kept()? {
            self.http
                .post(format!("{}/auth/token/revoke", self.api))
                .json(&json!({ "refresh_token": refresh }))
                .send()?
                .error_for_status()?;
        }
        self.forget()?;
        self.forget_access()?;
        eprintln!("Signed out.");
        Ok(())
    }

    // ---- Where the refresh token is kept ----

    /// `~/.config/virtx/<domain>.<extension>`.
    fn config(&self, extension: &str) -> anyhow::Result<PathBuf> {
        let home = std::env::var_os("HOME").context("no HOME to keep the session under")?;
        Ok(PathBuf::from(home)
            .join(".config/virtx")
            .join(format!("{}.{extension}", self.domain)))
    }

    /// Held until dropped: no other run of `virtx token` reads or replaces the session meanwhile.
    fn lock(&self) -> anyhow::Result<File> {
        let path = self.config("lock")?;
        std::fs::create_dir_all(path.parent().expect("a directory"))?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(file)
    }

    fn kept_access(&self) -> Option<Access> {
        let path = self.config("access").ok()?;
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    fn keep_access(&self, tokens: &Tokens) -> anyhow::Result<()> {
        let Some(lasts) = tokens.expires_in else {
            return self.forget_access();
        };
        let access = Access {
            token: tokens.access_token.clone(),
            expires_at: now() + lasts,
        };
        write_private(&self.config("access")?, &serde_json::to_vec(&access)?)
    }

    fn forget_access(&self) -> anyhow::Result<()> {
        match std::fs::remove_file(self.config("access")?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn entry(&self) -> Option<keyring::Entry> {
        keyring::Entry::new(KEYCHAIN, &self.domain).ok()
    }

    fn file(&self) -> anyhow::Result<PathBuf> {
        self.config("token")
    }

    fn keep(&self, refresh: &str) -> anyhow::Result<()> {
        if let Some(entry) = self.entry()
            && entry.set_password(refresh).is_ok()
        {
            return Ok(());
        }
        write_private(&self.file()?, refresh.as_bytes())
    }

    fn kept(&self) -> anyhow::Result<Option<String>> {
        if let Some(entry) = self.entry()
            && let Ok(refresh) = entry.get_password()
        {
            return Ok(Some(refresh));
        }
        match std::fs::read_to_string(self.file()?) {
            Ok(refresh) => Ok(Some(refresh.trim().to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn forget(&self) -> anyhow::Result<()> {
        if let Some(entry) = self.entry() {
            let _ = entry.delete_credential();
        }
        match std::fs::remove_file(self.file()?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Write `bytes` to `path`, readable by its owner alone.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().expect("a directory"))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(path)?, bytes)
        .with_context(|| format!("writing {}", path.display()))
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
