//! An OpenID Connect provider, as a public client signs in to it from a terminal: its
//! endpoints found through its discovery document, RFC 8628's device flow, and the refresh
//! and revocation of the session that makes.

use std::time::Duration;

use anyhow::{Context as _, bail};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Deserialize)]
struct Endpoints {
    issuer: String,
    token_endpoint: String,
    device_authorization_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
}

/// What the provider answers a device's request to sign in with.
#[derive(Deserialize)]
pub struct Codes {
    pub device_code: String,
    pub user_code: String,
    // Google's name for it.
    #[serde(alias = "verification_url")]
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    #[serde(default = "five")]
    pub interval: u64,
}

fn five() -> u64 {
    5
}

#[derive(Deserialize)]
pub struct Tokens {
    pub id_token: Option<String>,
    pub refresh_token: Option<String>,
}

/// What asking for a device's tokens came to.
pub enum Asked {
    Pending,
    SlowDown,
    Issued(Tokens),
}

pub struct Provider {
    http: reqwest::blocking::Client,
    client_id: String,
    client_secret: Option<String>,
    endpoints: Endpoints,
}

impl Provider {
    /// The provider at `issuer`, signed in to as `client_id`.
    pub fn discover(
        issuer: &str,
        client_id: &str,
        client_secret: Option<&str>,
    ) -> anyhow::Result<Provider> {
        let http = reqwest::blocking::Client::builder()
            .user_agent(concat!("virtx-cli/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()?;
        let url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let endpoints: Endpoints = http
            .get(&url)
            .send()
            .and_then(|answer| answer.error_for_status())
            .and_then(|answer| answer.json())
            .with_context(|| format!("reading {url}"))?;
        if endpoints.issuer != issuer {
            bail!("{url} is of {}, not {issuer}", endpoints.issuer);
        }
        Ok(Provider {
            http,
            client_id: client_id.to_owned(),
            client_secret: client_secret.map(str::to_owned),
            endpoints,
        })
    }

    /// `form`, with who the client is.
    fn post(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> reqwest::Result<reqwest::blocking::Response> {
        let mut form = form.to_vec();
        form.push(("client_id", &self.client_id));
        if let Some(secret) = &self.client_secret {
            form.push(("client_secret", secret));
        }
        self.http.post(url).form(&form).send()
    }

    /// The codes a person approves this device with, asking for `scope`.
    pub fn device(&self, scope: &str) -> anyhow::Result<Codes> {
        let url = self
            .endpoints
            .device_authorization_endpoint
            .as_deref()
            .context("the provider has no device flow")?;
        let answer = self.post(url, &[("scope", scope)])?;
        if !answer.status().is_success() {
            bail!("signing in failed: {}", answer.text().unwrap_or_default());
        }
        Ok(answer.json()?)
    }

    /// The tokens of the device `device_code` names, once a person approved it.
    pub fn ask(&self, device_code: &str) -> anyhow::Result<Asked> {
        let form = [("grant_type", DEVICE_GRANT), ("device_code", device_code)];
        let answer = self.post(&self.endpoints.token_endpoint, &form)?;
        if answer.status().is_success() {
            return Ok(Asked::Issued(answer.json()?));
        }
        let error: Value = answer.json().unwrap_or_default();
        match error["error"].as_str() {
            Some("authorization_pending") => Ok(Asked::Pending),
            Some("slow_down") => Ok(Asked::SlowDown),
            Some("access_denied") => bail!("the code was denied"),
            Some("expired_token") => bail!("the code expired; run virtx login again"),
            _ => bail!("signing in failed: {error}"),
        }
    }

    /// New tokens for the session `refresh` is of; `None` once the session has ended.
    pub fn refresh(&self, refresh: &str) -> anyhow::Result<Option<Tokens>> {
        let form = [("grant_type", "refresh_token"), ("refresh_token", refresh)];
        let answer = self.post(&self.endpoints.token_endpoint, &form)?;
        if answer.status().is_success() {
            return Ok(Some(answer.json()?));
        }
        let error: Value = answer.json().unwrap_or_default();
        match error["error"].as_str() {
            Some("invalid_grant") => Ok(None),
            _ => bail!("refreshing the session failed: {error}"),
        }
    }

    /// End the session `refresh` is of, where the provider takes that.
    pub fn revoke(&self, refresh: &str) {
        if let Some(url) = &self.endpoints.revocation_endpoint {
            let _ = self.post(url, &[("token", refresh)]);
        }
    }
}

/// The provider the server at `url` says to sign in to, and the client to sign in as
/// (`PROTOCOL.md`, Authentication); `None` from a server that names none.
pub fn advertised(url: &str) -> anyhow::Result<Option<(String, String)>> {
    let asked = format!("{}/auth", url.trim_end_matches('/'));
    let said: Value = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .get(&asked)
        .send()
        .and_then(|answer| answer.error_for_status())
        .and_then(|answer| answer.json())
        .with_context(|| format!("reading {asked}"))?;
    let oidc = &said["oidc"];
    Ok(
        match (oidc["issuer"].as_str(), oidc["client_id"].as_str()) {
            (Some(issuer), Some(client_id)) => Some((issuer.to_owned(), client_id.to_owned())),
            _ => None,
        },
    )
}

/// What `id_token` claims, unchecked: the server it is sent to checks it.
pub fn claims(id_token: &str) -> anyhow::Result<Value> {
    let payload = id_token.split('.').nth(1).context("not an ID token")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .context("not an ID token")?;
    serde_json::from_slice(&bytes).context("not an ID token")
}
