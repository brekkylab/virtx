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

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::routing::get;
    use jsonwebtoken::jwk::Jwk;
    use jsonwebtoken::{EncodingKey, Header};
    use serde_json::json;

    use super::*;

    const KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg2VGrzT1wOXvQVtaT\nL+shE0n9BR9/rn3Qn7RrXiJOcHKhRANCAAQCHMHdyNbFbHDPyRIdE7zhyTcL7XuZ\nlymVLSds5EVclDhHznH6nXaR6DUzWH+gad5rKfYNLAnnobLUNbrydOJS\n-----END PRIVATE KEY-----";

    /// A provider serving its discovery document and its one key, `k1`.
    pub(crate) struct Provider {
        pub issuer: String,
        /// How many times its keys were asked for.
        pub fetched: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Provider {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Provider {
        pub(crate) async fn spawn() -> Provider {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let issuer = format!("http://{}", listener.local_addr().unwrap());
            let key = EncodingKey::from_ec_pem(KEY.as_bytes()).unwrap();
            let mut jwk = Jwk::from_encoding_key(&key, Algorithm::ES256).unwrap();
            jwk.common.key_id = Some("k1".into());
            let keys = json!(JwkSet { keys: vec![jwk] });
            let discovery = json!({ "issuer": issuer, "jwks_uri": format!("{issuer}/keys") });
            let fetched = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&fetched);
            let router = axum::Router::new()
                .route(
                    "/.well-known/openid-configuration",
                    get(move || async move { discovery.to_string() }),
                )
                .route(
                    "/keys",
                    get(move || {
                        counted.fetch_add(1, Ordering::Relaxed);
                        async move { keys.to_string() }
                    }),
                );
            let task = tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            Provider {
                issuer,
                fetched,
                task,
            }
        }

        /// A token of this provider's for client `virtx`, good for a minute, with `claims`.
        pub(crate) fn token(&self, claims: Value) -> String {
            signed("k1", &self.issuer, "virtx", claims, 60)
        }
    }

    pub(crate) fn signed(
        kid: &str,
        iss: &str,
        aud: &str,
        mut claims: Value,
        exp_in: i64,
    ) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        claims["iss"] = iss.into();
        claims["aud"] = aud.into();
        claims["iat"] = now.into();
        claims["exp"] = (now + exp_in).into();
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(kid.into());
        let key = EncodingKey::from_ec_pem(KEY.as_bytes()).unwrap();
        jsonwebtoken::encode(&header, &claims, &key).unwrap()
    }

    fn oidc(provider: &Provider, claim: &str) -> Oidc {
        Oidc::new(Config {
            issuer: provider.issuer.clone(),
            client_id: "virtx".into(),
            username_claim: claim.into(),
        })
    }

    #[tokio::test]
    async fn a_token_the_provider_signed_for_the_client_names_its_user() {
        let provider = Provider::spawn().await;
        let oidc = oidc(&provider, "email");
        let alice = json!({ "sub": "u-1", "email": "alice@example.com" });

        let user = oidc.user(&provider.token(alice.clone())).await;
        assert_eq!(user.as_deref(), Some("alice@example.com"));
        let by_sub = self::oidc(&provider, "sub");
        let user = by_sub.user(&provider.token(alice.clone())).await;
        assert_eq!(user.as_deref(), Some("u-1"));

        let issuer = &provider.issuer;
        let refused = [
            // Another client's, another provider's, one run out, and one without the claim.
            signed("k1", issuer, "other", alice.clone(), 60),
            signed("k1", "http://127.0.0.1:9", "virtx", alice.clone(), 60),
            signed("k1", issuer, "virtx", alice.clone(), -120),
            provider.token(json!({ "sub": "u-1" })),
            provider.token(json!({ "email": "alice@example.com", "email_verified": false })),
            "not-a-token".to_owned(),
        ];
        for token in &refused {
            assert_eq!(oidc.user(token).await, None, "{token}");
        }
    }

    #[tokio::test]
    async fn a_token_signed_with_a_shared_secret_names_no_one() {
        let provider = Provider::spawn().await;
        let oidc = oidc(&provider, "sub");
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("k1".into());
        let claims =
            json!({ "iss": provider.issuer, "aud": "virtx", "sub": "u-1", "exp": u32::MAX });
        let key = EncodingKey::from_secret(b"guessed");
        let token = jsonwebtoken::encode(&header, &claims, &key).unwrap();
        assert_eq!(oidc.user(&token).await, None);
    }

    #[tokio::test]
    async fn the_keys_are_fetched_once_and_not_again_for_each_key_unknown() {
        let provider = Provider::spawn().await;
        let oidc = oidc(&provider, "sub");
        let good = provider.token(json!({ "sub": "u-1" }));
        let unknown = signed("k2", &provider.issuer, "virtx", json!({ "sub": "u-1" }), 60);

        assert!(oidc.user(&good).await.is_some());
        assert!(oidc.user(&good).await.is_some());
        assert_eq!(oidc.user(&unknown).await, None);
        assert_eq!(oidc.user(&unknown).await, None);
        assert_eq!(provider.fetched.load(Ordering::Relaxed), 1);
    }
}
