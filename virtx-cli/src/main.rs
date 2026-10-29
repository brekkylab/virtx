//! `virtx`: signing in to virtx cloud from a terminal, for the SDKs to use.
//!
//! ```text
//! virtx login     sign in: approve the code it shows in the console, in a browser
//! virtx token     print an access token, refreshed as needed (VIRTX_API_KEY takes it)
//! virtx whoami    who is signed in
//! virtx logout    sign out: the session ends
//! ```
//!
//! Signing in is RFC 8628's device flow against the control plane at `https://api.<domain>`,
//! `<domain>` being `--domain`, `VIRTX_DOMAIN`, or [`DOMAIN`]. The session's refresh token is
//! kept in the OS's keychain, under the domain; where the OS has none, in
//! `~/.config/virtx/<domain>.token`, readable by its owner alone. An access token lasts 15
//! minutes, and `virtx token` gives a fresh one each time.
//!
//! With an access token, a client names its project as well -- `VIRTX_PROJECT` -- and its
//! cell's endpoint, `VIRTX_URL`: unlike an API key, a person's token names neither.

use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::{Value, json};

/// Where virtx cloud is, by default, as the SDKs have it.
const DOMAIN: &str = "virtx.brekkylab.com";
const KEYCHAIN: &str = "virtx";

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
    Token,
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

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

struct Cloud {
    api: String,
    domain: String,
    http: reqwest::blocking::Client,
}

fn main() -> anyhow::Result<()> {
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
        Command::Token => {
            println!("{}", cloud.token()?);
            Ok(())
        }
        Command::Whoami => {
            let me = cloud.me(&cloud.token()?)?;
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
                let refresh = tokens.refresh_token.context("no refresh token answered")?;
                self.keep(&refresh)?;
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

    /// An access token, from the session kept: refreshed, and the refresh token replaced.
    fn token(&self) -> anyhow::Result<String> {
        let refresh = self.kept()?.context("not signed in; run virtx login")?;
        let answer = self
            .http
            .post(format!("{}/auth/token", self.api))
            .json(&json!({ "refresh_token": refresh }))
            .send()?;
        if answer.status() == reqwest::StatusCode::UNAUTHORIZED {
            let _ = self.forget();
            bail!("the session has ended; run virtx login");
        }
        let tokens: Tokens = answer.error_for_status()?.json()?;
        if let Some(refresh) = &tokens.refresh_token {
            self.keep(refresh)?;
        }
        Ok(tokens.access_token)
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
        eprintln!("Signed out.");
        Ok(())
    }

    // ---- Where the refresh token is kept ----

    fn entry(&self) -> Option<keyring::Entry> {
        keyring::Entry::new(KEYCHAIN, &self.domain).ok()
    }

    fn file(&self) -> anyhow::Result<PathBuf> {
        let home = std::env::var_os("HOME").context("no HOME to keep the session under")?;
        Ok(PathBuf::from(home)
            .join(".config/virtx")
            .join(format!("{}.token", self.domain)))
    }

    fn keep(&self, refresh: &str) -> anyhow::Result<()> {
        if let Some(entry) = self.entry()
            && entry.set_password(refresh).is_ok()
        {
            return Ok(());
        }
        let path = self.file()?;
        std::fs::create_dir_all(path.parent().expect("a directory"))?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        std::io::Write::write_all(&mut options.open(&path)?, refresh.as_bytes())
            .with_context(|| format!("writing {}", path.display()))
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
