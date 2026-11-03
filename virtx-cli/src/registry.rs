//! Credentials for the registry a pull or push goes to, read from stdin so no secret is on the
//! command line.

use std::io::Read;

use anyhow::Context as _;
use clap::Args;
use virtx_core::RegistryAuth;

#[derive(Debug, Default, PartialEq, Args)]
pub(crate) struct RegistryArgs {
    /// The registry's username; its password, or an access token, is read from stdin.
    #[arg(short, long, value_name = "USER")]
    pub(crate) username: Option<String>,

    /// Read a bearer token for the registry from stdin.
    #[arg(long, conflicts_with = "username")]
    pub(crate) token_stdin: bool,
}

impl RegistryArgs {
    /// The credentials asked for, if any; none goes to the registry anonymously.
    pub(crate) fn auth(&self) -> anyhow::Result<Option<RegistryAuth>> {
        self.auth_from(std::io::stdin())
    }

    fn auth_from(&self, stdin: impl Read) -> anyhow::Result<Option<RegistryAuth>> {
        if self.username.is_none() && !self.token_stdin {
            return Ok(None);
        }
        let secret = secret(stdin)?;
        Ok(Some(match &self.username {
            Some(username) => RegistryAuth::Basic {
                username: username.clone(),
                password: secret,
            },
            None => RegistryAuth::Bearer { token: secret },
        }))
    }
}

/// What stdin holds, without its line ending, as `echo $TOKEN |` sends it.
fn secret(mut stdin: impl Read) -> anyhow::Result<String> {
    let mut text = String::new();
    stdin
        .read_to_string(&mut text)
        .context("reading the registry's secret from stdin")?;
    let secret = text.trim_end_matches(['\r', '\n']);
    anyhow::ensure!(!secret.is_empty(), "stdin held no secret for the registry");
    Ok(secret.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_username_takes_its_password_and_a_token_stands_alone() {
        let basic = RegistryArgs {
            username: Some("me".into()),
            token_stdin: false,
        };
        assert_eq!(
            basic.auth_from("hunter2\n".as_bytes()).unwrap(),
            Some(RegistryAuth::Basic {
                username: "me".into(),
                password: "hunter2".into()
            })
        );
        let bearer = RegistryArgs {
            username: None,
            token_stdin: true,
        };
        assert_eq!(
            bearer.auth_from("tok\r\n".as_bytes()).unwrap(),
            Some(RegistryAuth::Bearer {
                token: "tok".into()
            })
        );
        assert!(bearer.auth_from("\n".as_bytes()).is_err());
    }

    #[test]
    fn without_either_stdin_is_not_read() {
        let anonymous = RegistryArgs::default();
        assert_eq!(anonymous.auth_from(std::io::empty()).unwrap(), None);
    }
}
