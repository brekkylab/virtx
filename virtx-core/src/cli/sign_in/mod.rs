//! Signing in to a virtx server's OpenID Connect provider from a terminal, for the SDKs to use:
//!
//! ```text
//! virtx login [url]   sign in to the server at `url`: approve the code it shows, in a browser
//! virtx token         print an ID token, refreshed as needed; --json, with how long it
//!                     lasts, as the SDKs run it for a token
//! virtx whoami        who is signed in
//! virtx logout        sign out: the session ends
//! ```
//!
//! Signing in is RFC 8628's device flow against the provider the server at `url` names, as
//! the public client it names (the protocol, Authentication) -- or those `--issuer`
//! (`VIRTX_OIDC_ISSUER`) and `--client-id` (`VIRTX_OIDC_CLIENT_ID`) name, the `issuer` and
//! `client_id` of the server's `oidc`. The token is the ID token, which the server checks
//! against the provider's keys. The other commands are of the provider last signed in to,
//! unless given one.
//!
//! Many processes may ask for a token at once -- an SDK runs `virtx token --json` in each --
//! and a provider that replaces the refresh token with each refresh takes one presented twice
//! for stolen. So `virtx token` holds a lock while it reads, refreshes and keeps, and gives
//! the ID token it last got for as long as that has more than `FRESH_FOR` left: one refresh
//! serves them all. Run with `TOKEN_REFUSED` set, as an SDK runs it once a server
//! refused the token it had, it refreshes whatever it has.
//!
//! `command.rs` is the commands and how each runs, `provider.rs` the provider, and `session.rs`
//! what is kept between runs. The commands are the `virtx` parser's own, and run before any
//! runtime starts, since their HTTP blocks.

mod command;
mod provider;
mod session;

pub(super) use command::{ProviderArgs, SignInCommand, run};
