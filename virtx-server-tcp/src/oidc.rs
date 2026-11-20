//! The users a token issuer names: a request's token may be a token the config's `oidc` issuer
//! signed for its client, and the user is the claim the config picks. The issuer is an OpenID
//! Connect provider, or any whose keys are at a URL the config gives.
//!
//! The issuer's keys are found through its discovery document, or at the config's `jwks_uri`,
//! and kept; a token signed with a key not among them has them fetched again, at most once in
//! [`REFETCH_AFTER`], or in [`RETRY_AFTER`] after a fetch failed.

use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

use crate::access::Principal;

/// How long the keys last fetched stand before a key unknown has them fetched again.
const REFETCH_AFTER: Duration = Duration::from_secs(60);

/// How long after a failed fetch a key unknown has them fetched again: a provider briefly
/// down leaves every token refused no longer than this.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// The config's `oidc`.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The issuer, as its tokens' `iss` has it: its discovery document is under it.
    pub issuer: String,
    /// The client the tokens are for: their `aud`.
    pub client_id: String,
    /// Where the issuer's keys are, as a JWK set: in place of its discovery document, for an
    /// issuer that has none.
    #[serde(default)]
    pub jwks_uri: Option<String>,
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

    /// Who `token` is from, and what it carries, if the provider signed it for the client and
    /// it is current.
    pub async fn principal(&self, token: &str) -> Option<Principal> {
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
        let user = claims.get(claim)?.as_str()?.to_owned();
        let Value::Object(claims) = claims else {
            return None;
        };
        Some(Principal { user, claims })
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
        if let Some(keys) = &self.config.jwks_uri {
            return self.get(keys).await.context("its keys");
        }
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
mod tests {
    use std::time::SystemTime;

    use jsonwebtoken::{EncodingKey, Header};
    use serde_json::json;

    use super::*;

    /// A P-256 key, and its public half as a JWK's `x` and `y`.
    const KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgTFNNLGR4z9BOxmmG
kWTAwIsXQv7M5AnnFYbw7KqIAduhRANCAARm+2fsK24SWK8QJjqyNzhrBvlhw8UG
rABUOPQZw+SIzclThn/7Vcn7i0rAcmmlVj8sdMeJsFeMZ3CehtnTDGVK
-----END PRIVATE KEY-----
";
    const X: &str = "Zvtn7CtuElivECY6sjc4awb5YcPFBqwAVDj0GcPkiM0";
    const Y: &str = "yVOGf_tVyfuLSsByaaVWPyx0x4mwV4xncJ6G2dMMZUo";

    #[tokio::test]
    async fn an_issuer_with_no_discovery_document_is_taken_at_its_keys() {
        let keys = json!({"keys": [{
            "kty": "EC", "crv": "P-256", "kid": "k1", "alg": "ES256", "use": "sig", "x": X, "y": Y,
        }]});
        let served = axum::Router::new().route(
            "/keys",
            axum::routing::get(move || async move { keys.to_string() }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/keys", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, served).await });

        let issuer = "https://gateway.example";
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some("k1".into());
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();
        let claims =
            json!({"iss": issuer, "aud": "node", "exp": now.as_secs() + 600, "sub": "agent"});
        let key = EncodingKey::from_ec_pem(KEY.as_bytes()).unwrap();
        let token = jsonwebtoken::encode(&header, &claims, &key).unwrap();
        let oidc = Oidc::new(Config {
            issuer: issuer.into(),
            client_id: "node".into(),
            username_claim: "sub".into(),
            jwks_uri: Some(url),
        });
        let principal = oidc.principal(&token).await.unwrap();
        assert_eq!(principal.user, "agent");
        assert_eq!(principal.claims["aud"], "node");
        task.abort();
    }

    #[tokio::test]
    async fn a_token_of_another_issuer_or_keys_another_issuer_names_are_refused() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = format!("http://{}", listener.local_addr().unwrap());
        let keys = json!({"keys": [{
            "kty": "EC", "crv": "P-256", "kid": "k1", "alg": "ES256", "use": "sig", "x": X, "y": Y,
        }]});
        // Two issuers here, both with the keys here: one whose discovery document names it, one
        // whose names another.
        let discovery = |issuer: String| {
            let document = json!({"issuer": issuer, "jwks_uri": format!("{at}/keys")});
            axum::routing::get(move || async move { document.to_string() })
        };
        let served = axum::Router::new()
            .route(
                "/keys",
                axum::routing::get(move || async move { keys.to_string() }),
            )
            .route(
                "/own/.well-known/openid-configuration",
                discovery(format!("{at}/own")),
            )
            .route(
                "/another/.well-known/openid-configuration",
                discovery("https://elsewhere.example".into()),
            );
        let task = tokio::spawn(async move { axum::serve(listener, served).await });

        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some("k1".into());
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();
        let key = EncodingKey::from_ec_pem(KEY.as_bytes()).unwrap();
        let signed = |issuer: &str| {
            let claims =
                json!({"iss": issuer, "aud": "node", "exp": now.as_secs() + 600, "sub": "agent"});
            jsonwebtoken::encode(&header, &claims, &key).unwrap()
        };
        let oidc = |issuer: String| {
            Oidc::new(Config {
                issuer,
                client_id: "node".into(),
                username_claim: "sub".into(),
                jwks_uri: None,
            })
        };

        let own = oidc(format!("{at}/own"));
        assert!(own.principal(&signed(&format!("{at}/own"))).await.is_some());
        assert!(
            own.principal(&signed(&format!("{at}/another")))
                .await
                .is_none()
        );
        // Its keys would check the token, but the document they are named in is another's.
        let another = oidc(format!("{at}/another"));
        assert!(
            another
                .principal(&signed(&format!("{at}/another")))
                .await
                .is_none()
        );
        task.abort();
    }
}
