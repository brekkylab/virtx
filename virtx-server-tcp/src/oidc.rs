//! The users an OpenID Connect provider names: a request's token may be an ID token the
//! config's `oidc` issuer signed for its client, and the user is the claim the config picks.
//!
//! The provider's keys are found through its discovery document and kept; a token signed with
//! a key not among them has them fetched again, at most once in [`REFETCH_AFTER`].

use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::RwLock;

/// How long the keys last fetched stand before a key unknown has them fetched again.
const REFETCH_AFTER: Duration = Duration::from_secs(60);

/// The config's `oidc`.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The provider, as its tokens' `iss` has it: its discovery document is under it.
    pub issuer: String,
    /// The client the tokens are for: their `aud`.
    pub client_id: String,
    /// The claim that names the user.
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

pub struct Oidc {
    config: Config,
    http: reqwest::Client,
    /// The provider's keys, and when they were last asked for.
    keys: RwLock<(JwkSet, Option<Instant>)>,
}

impl Oidc {
    pub fn new(config: Config) -> Oidc {
        Oidc {
            config,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("an HTTP client"),
            keys: RwLock::new((JwkSet { keys: Vec::new() }, None)),
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
        // An address the provider has not verified is whatever its holder typed.
        if claim == "email" && claims.get("email_verified") == Some(&Value::Bool(false)) {
            return None;
        }
        claims.get(claim)?.as_str().map(str::to_owned)
    }

    async fn key(&self, kid: &str) -> Option<DecodingKey> {
        {
            let (keys, fetched) = &*self.keys.read().await;
            if let Some(jwk) = keys.find(kid) {
                return DecodingKey::from_jwk(jwk).ok();
            }
            if fetched.is_some_and(|at| at.elapsed() < REFETCH_AFTER) {
                return None;
            }
        }
        let mut keys = self.keys.write().await;
        if keys.1.is_none_or(|at| at.elapsed() >= REFETCH_AFTER) {
            keys.1 = Some(Instant::now());
            match self.fetch().await {
                Ok(fetched) => keys.0 = fetched,
                Err(e) => eprintln!("fetching the keys of {}: {e:#}", self.config.issuer),
            }
        }
        DecodingKey::from_jwk(keys.0.find(kid)?).ok()
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
