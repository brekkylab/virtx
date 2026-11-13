//! virtx cloud from a terminal, for the SDKs to use:
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
//! `<domain>` being `--domain`, `VIRTX_DOMAIN`, or [`DOMAIN`]. The session is kept in files, each
//! its owner's alone: the refresh token in `~/.config/virtx/<domain>.token`, the last access
//! token in `<domain>.access`. Files, not the OS's keychain, so that the `virtx` the Python and
//! Node packages ship signs in as the one built from this repository does, and an SDK's
//! `Client.from_env()` -- `VIRTX_ORG` and no `VIRTX_API_KEY` -- finds a token wherever it runs
//! (issue #158). An access token lasts 15 minutes.
//!
//! Many processes may ask for a token at once -- an SDK runs `virtx token --json` in each, as
//! an agent's workers start together -- and each refresh replaces the refresh token: two at
//! once would present one already replaced, which the control plane takes for a stolen token
//! and ends the session over. So `virtx token` holds `~/.config/virtx/<domain>.lock` while it
//! reads, refreshes and keeps, and gives the access token it last got for as long as it has
//! more than [`FRESH_FOR`] left: one refresh serves them all. Run with `VIRTX_TOKEN_REFUSED`
//! set, as an SDK runs it once a server refused the token it had, it refreshes whatever it has.
//!
//! With an access token, a client names its organization as well -- `VIRTX_ORG`, its UUID --
//! and its cell's endpoint, `VIRTX_URL`: unlike an API key, a person's token names neither.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Where virtx cloud is, by default, as the SDKs have it.
pub(crate) const DOMAIN: &str = "virtx.sh";
/// How long an access token kept has to have left to be given again: more than the minute
/// before it runs out in which an SDK asks for another.
const FRESH_FOR: Duration = Duration::from_secs(120);

#[derive(Debug, PartialEq, Args)]
pub(crate) struct CloudArgs {
    /// Where virtx cloud is: its control plane is api.<domain>. For `login`, `token`,
    /// `whoami` and `logout`.
    #[arg(long, global = true, env = "VIRTX_DOMAIN", default_value = DOMAIN)]
    pub(crate) domain: String,
}

#[derive(Debug, PartialEq, Subcommand)]
pub(crate) enum CloudCommand {
    /// Sign in to virtx cloud, approving the code shown in the console.
    Login,
    /// Print an access token to virtx cloud, refreshed.
    Token {
        /// Print `{"token": ..., "expires_in": <seconds>}`, as the SDKs read it.
        #[arg(long)]
        json: bool,
    },
    /// Say who is signed in to virtx cloud.
    Whoami,
    /// Sign out of virtx cloud, ending the session.
    Logout,
}

/// Run `command` against virtx cloud at `domain`.
pub(crate) async fn run(domain: &str, command: CloudCommand) -> anyhow::Result<()> {
    let config = std::env::home_dir()
        .context("no home directory to keep the session under")?
        .join(".config/virtx");
    let cloud = Cloud {
        api: format!("https://api.{domain}"),
        domain: domain.to_owned(),
        config,
        http: http()?,
    };
    match command {
        CloudCommand::Login => cloud.login().await,
        CloudCommand::Token { json } => {
            let tokens = cloud.token().await?;
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
        CloudCommand::Whoami => {
            let me = cloud.me(&cloud.token().await?.access_token).await?;
            println!("{}", me["user"]["email"].as_str().unwrap_or("?"));
            Ok(())
        }
        CloudCommand::Logout => cloud.logout().await,
    }
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
    /// Where the session is kept: `~/.config/virtx`.
    config: PathBuf,
    http: reqwest::Client,
}

/// The client virtx cloud is asked with. It follows no redirect: the refresh token travels in a
/// request's body, which a `307` or `308` would carry on to wherever it pointed -- to another
/// host, or over plain HTTP (issue #170). A redirect is answered as the error it is
/// ([`checked`]).
fn http() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("virtx/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// `answer`, virtx cloud's, if it is a success: a redirect, which is not followed ([`http`]),
/// and any other status are errors.
fn checked(answer: reqwest::Response) -> anyhow::Result<reqwest::Response> {
    if answer.status().is_redirection() {
        bail!(
            "{} answered {}, a redirect, which is not followed",
            answer.url(),
            answer.status()
        );
    }
    Ok(answer.error_for_status()?)
}

/// `answer`'s body, as JSON.
async fn json_of<T: serde::de::DeserializeOwned>(answer: reqwest::Response) -> anyhow::Result<T> {
    let bytes = answer.bytes().await?;
    Ok(serde_json::from_slice(&bytes)?)
}

impl Cloud {
    /// `POST <api><path>` with `body` as JSON.
    async fn post(&self, path: &str, body: &Value) -> anyhow::Result<reqwest::Response> {
        Ok(self
            .http
            .post(format!("{}{path}", self.api))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(body)?)
            .send()
            .await?)
    }

    async fn login(&self) -> anyhow::Result<()> {
        let codes: Codes = json_of(checked(self.post("/auth/device", &json!({})).await?)?).await?;
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
            tokio::time::sleep(interval).await;
            let answer = self
                .post(
                    "/auth/device/token",
                    &json!({ "device_code": codes.device_code }),
                )
                .await?;
            if answer.status().is_success() {
                let tokens: Tokens = json_of(answer).await?;
                let refresh = tokens
                    .refresh_token
                    .as_deref()
                    .context("no refresh token answered")?;
                let _lock = self.lock()?;
                self.keep(refresh)?;
                self.keep_access(&tokens)?;
                let me = self.me(&tokens.access_token).await?;
                eprintln!(
                    "Signed in as {}.",
                    me["user"]["email"].as_str().unwrap_or("?")
                );
                return Ok(());
            }
            let error: Value = json_of(answer).await.unwrap_or_default();
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
    async fn token(&self) -> anyhow::Result<Tokens> {
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
        let Some(tokens) = self.refreshed(&refresh).await? else {
            // Kept as it is: under the lock no other run of this replaced it, so the session
            // has ended, and `virtx login` puts a new one in its place.
            let _ = self.forget_access();
            bail!("the session has ended; run virtx login");
        };
        if let Some(refresh) = &tokens.refresh_token {
            self.keep(refresh)?;
        }
        self.keep_access(&tokens)?;
        Ok(tokens)
    }

    /// The tokens `refresh` is replaced with -- or none, `401`, for a session that has ended.
    async fn refreshed(&self, refresh: &str) -> anyhow::Result<Option<Tokens>> {
        let answer = self
            .post("/auth/token", &json!({ "refresh_token": refresh }))
            .await?;
        if answer.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Ok(None);
        }
        Ok(Some(json_of(checked(answer)?).await?))
    }

    async fn me(&self, access: &str) -> anyhow::Result<Value> {
        let answer = self
            .http
            .get(format!("{}/v1/me", self.api))
            .bearer_auth(access)
            .send()
            .await?;
        json_of(checked(answer)?).await
    }

    /// Signed out: the session ended at virtx cloud, and forgotten here -- here whatever virtx
    /// cloud answers, so a machine signed out of is signed out of even offline (issue #163).
    /// One virtx cloud did not end is said, and is an error: a copy of its token taken
    /// before stays usable until it expires.
    async fn logout(&self) -> anyhow::Result<()> {
        let _lock = self.lock()?;
        let revoked = match self.kept()? {
            Some(refresh) => match self
                .post("/auth/token/revoke", &json!({ "refresh_token": refresh }))
                .await
            {
                Ok(answer) => checked(answer).map(drop),
                Err(e) => Err(e),
            },
            None => Ok(()),
        };
        self.forget()?;
        self.forget_access()?;
        match revoked {
            Ok(()) => {
                eprintln!("Signed out.");
                Ok(())
            }
            Err(e) => Err(e.context(
                "signed out here, but virtx cloud could not be told to end the session: it \
                 ends as it expires",
            )),
        }
    }

    // ---- Where the session is kept ----

    /// `~/.config/virtx/<domain>.<extension>`.
    fn config(&self, extension: &str) -> PathBuf {
        self.config.join(format!("{}.{extension}", self.domain))
    }

    /// Held until dropped: no other run of `virtx token` reads or replaces the session meanwhile.
    fn lock(&self) -> anyhow::Result<File> {
        let path = self.config("lock");
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
        serde_json::from_slice(&std::fs::read(self.config("access")).ok()?).ok()
    }

    fn keep_access(&self, tokens: &Tokens) -> anyhow::Result<()> {
        let Some(lasts) = tokens.expires_in else {
            return self.forget_access();
        };
        let access = Access {
            token: tokens.access_token.clone(),
            expires_at: now() + lasts,
        };
        write_private(&self.config("access"), &serde_json::to_vec(&access)?)
    }

    fn forget_access(&self) -> anyhow::Result<()> {
        removed(&self.config("access"))
    }

    fn keep(&self, refresh: &str) -> anyhow::Result<()> {
        write_private(&self.config("token"), refresh.as_bytes())
    }

    fn kept(&self) -> anyhow::Result<Option<String>> {
        match std::fs::read_to_string(self.config("token")) {
            Ok(refresh) => Ok(Some(refresh.trim().to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn forget(&self) -> anyhow::Result<()> {
        removed(&self.config("token"))
    }
}

/// `path` removed, if it was there.
fn removed(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Write `bytes` to `path`, readable by its owner alone -- however a file there was left: written
/// to a new file beside it, made so, and put in its place (issue #170). On Windows the file has
/// its directory's permissions, the user's profile's.
fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().expect("a directory");
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().expect("a file").to_string_lossy();
    let fresh = dir.join(format!(".{name}.{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let _ = std::fs::remove_file(&fresh);
    let written = options
        .open(&fresh)
        .and_then(|mut file| std::io::Write::write_all(&mut file, bytes))
        .and_then(|()| std::fs::rename(&fresh, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&fresh);
    }
    written.with_context(|| format!("writing {}", path.display()))
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
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::TcpListener;

    use super::*;

    /// A directory of its own under the system's temporary one, empty.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("virtx-cloud-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// virtx cloud at `api`, its session kept under `config`.
    fn cloud(api: &str, config: PathBuf) -> Cloud {
        Cloud {
            api: api.to_owned(),
            domain: "qa.invalid".into(),
            config,
            http: http().unwrap(),
        }
    }

    /// One HTTP request taken on `listener` and answered `answer`; its request line.
    fn answer_once(listener: TcpListener, answer: String) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if let Some(n) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = n.trim().parse().unwrap();
                }
                if header == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            stream.write_all(answer.as_bytes()).unwrap();
            line
        })
    }

    /// A token file left readable by others is its owner's alone once written again (issue #170).
    #[cfg(unix)]
    #[test]
    fn a_token_file_is_its_owner_s_alone_however_it_was_left() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = scratch("mode");
        let path = dir.join("qa.token");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"new").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "nothing left beside it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A refresh answered with a redirect is an error, and the refresh token is carried nowhere
    /// it points (issue #170).
    #[tokio::test]
    async fn a_refresh_token_is_never_carried_where_a_redirect_points() {
        let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
        let there = elsewhere.local_addr().unwrap();
        elsewhere.set_nonblocking(true).unwrap();
        let api = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = format!("http://{}", api.local_addr().unwrap());
        let asked = answer_once(
            api,
            format!(
                "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://{there}/elsewhere\r\n\
                 content-length: 0\r\nconnection: close\r\n\r\n"
            ),
        );
        let refreshed = cloud(&at, scratch("redirect"))
            .refreshed("made-up-refresh-token")
            .await;
        assert!(refreshed.is_err(), "{:?}", refreshed.map(|t| t.is_some()));
        assert!(asked.join().unwrap().starts_with("POST /auth/token "));
        assert!(
            elsewhere.accept().is_err(),
            "nothing reached where the redirect pointed"
        );
    }

    /// Signing out forgets the session here whatever virtx cloud answers -- here, nothing, its
    /// address refusing connections -- and says it was not ended there (issue #163).
    #[tokio::test]
    async fn signing_out_forgets_the_session_even_where_virtx_cloud_is_not_reached() {
        let dir = scratch("logout");
        let gone = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = format!("http://{}", gone.local_addr().unwrap());
        drop(gone);
        let cloud = cloud(&at, dir.clone());
        write_private(&cloud.config("token"), b"made-up-refresh-token").unwrap();
        write_private(&cloud.config("access"), b"{}").unwrap();
        let out = cloud.logout().await;
        assert!(out.is_err(), "the session not ended at virtx cloud is said");
        assert!(
            !cloud.config("token").exists(),
            "its refresh token forgotten"
        );
        assert!(
            !cloud.config("access").exists(),
            "its access token forgotten"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `virtx token`, against virtx cloud answering a refresh: the access token printed is the
    /// one answered, and the session kept in files -- the refresh token replaced, the access
    /// token kept for the next run, which is given it without asking (issue #158: the session
    /// the packages' `virtx` keeps is its own, with no keychain).
    #[tokio::test]
    async fn a_token_is_refreshed_from_the_session_kept_in_files_and_kept_for_the_next() {
        let dir = scratch("token");
        let api = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = format!("http://{}", api.local_addr().unwrap());
        let body =
            r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":900}"#;
        let asked = answer_once(
            api,
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            ),
        );
        let cloud = cloud(&at, dir.clone());
        write_private(&cloud.config("token"), b"old-refresh").unwrap();
        let tokens = cloud.token().await.unwrap();
        assert_eq!(tokens.access_token, "new-access");
        assert!(asked.join().unwrap().starts_with("POST /auth/token "));
        assert_eq!(
            std::fs::read_to_string(cloud.config("token")).unwrap(),
            "new-refresh"
        );
        // The next run: given the one kept, virtx cloud not asked (nothing listens there now).
        assert_eq!(cloud.token().await.unwrap().access_token, "new-access");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
