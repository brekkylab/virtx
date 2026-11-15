//! The users an OpenID Connect provider names: a request's token may be an ID token the
//! config's `oidc` issuer signed for its client, and the user is the claim the config picks.
//!
//! The provider's keys are found through its discovery document and kept; a token signed with
//! a key not among them has them fetched again, at most once in [`REFETCH_AFTER`], or in
//! [`RETRY_AFTER`] after a fetch failed.

use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

/// How long the keys last fetched stand before a key unknown has them fetched again.
const REFETCH_AFTER: Duration = Duration::from_secs(60);

/// How long after a failed fetch a key unknown has them fetched again: a provider briefly
/// down leaves every token refused no longer than this.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// The config's `oidc`.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The provider, as its tokens' `iss` has it: its discovery document is under it.
    pub issuer: String,
    /// The client the tokens are for: their `aud`.
    pub client_id: String,
    /// The claim that names the user. `email` is taken only with `email_verified` `true`.
    #[serde(default = "email")]
    pub username_claim: String,
}

fn email() -> String {
    "email".to_owned()
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
}

/// The provider's keys last fetched.
struct Keys {
    set: JwkSet,
    /// When a key not among them may have them fetched again.
    due: Instant,
}

impl Keys {
    fn key(&self, kid: &str) -> Option<DecodingKey> {
        DecodingKey::from_jwk(self.set.find(kid)?).ok()
    }
}

pub struct Oidc {
    config: Config,
    http: reqwest::Client,
    keys: RwLock<Keys>,
    /// Held while the keys are fetched, so one fetch runs at a time.
    fetching: Mutex<()>,
}

impl Oidc {
    pub fn new(config: Config) -> Oidc {
        Oidc {
            config,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("an HTTP client"),
            keys: RwLock::new(Keys {
                set: JwkSet { keys: Vec::new() },
                due: Instant::now(),
            }),
            fetching: Mutex::new(()),
        }
    }

    /// The user `token` names, if the provider signed it for the client and it is current.
    pub async fn user(&self, token: &str) -> Option<String> {
        let header = jsonwebtoken::decode_header(token).ok()?;
        // Signed with a key pair only: a shared secret is no provider's signature.
        if matches!(
            header.alg,
            Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512
        ) {
            return None;
        }
        let key = self.key(&header.kid?).await?;
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[&self.config.issuer]);
        validation.set_audience(&[&self.config.client_id]);
        let claims = jsonwebtoken::decode::<Value>(token, &key, &validation)
            .ok()?
            .claims;
        let claim = &self.config.username_claim;
        // An address the provider does not say it verified is whatever its holder typed.
        if claim == "email" && claims.get("email_verified") != Some(&Value::Bool(true)) {
            return None;
        }
        claims.get(claim)?.as_str().map(str::to_owned)
    }

    async fn key(&self, kid: &str) -> Option<DecodingKey> {
        if let Some(key) = self.keys.read().await.key(kid) {
            return Some(key);
        }
        // Fetched outside `keys`, so tokens of keys known are checked meanwhile.
        let _fetching = self.fetching.lock().await;
        {
            let keys = self.keys.read().await;
            if let Some(key) = keys.key(kid) {
                return Some(key);
            }
            if Instant::now() < keys.due {
                return None;
            }
        }
        let fetched = self.fetch().await;
        let mut keys = self.keys.write().await;
        match fetched {
            Ok(set) => {
                keys.set = set;
                keys.due = Instant::now() + REFETCH_AFTER;
            }
            Err(e) => {
                eprintln!("fetching the keys of {}: {e:#}", self.config.issuer);
                keys.due = Instant::now() + RETRY_AFTER;
            }
        }
        keys.key(kid)
    }

    async fn fetch(&self) -> anyhow::Result<JwkSet> {
        let issuer = self.config.issuer.trim_end_matches('/');
        let url = format!("{issuer}/.well-known/openid-configuration");
        let discovery: Discovery = self.get(&url).await.context("its discovery document")?;
        if discovery.issuer != self.config.issuer {
            bail!("its discovery document is of {}", discovery.issuer);
        }
        self.get(&discovery.jwks_uri).await.context("its keys")
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> anyhow::Result<T> {
        let response = self.http.get(url).send().await?.error_for_status()?;
        Ok(response.json().await?)
    }
}
