//! A virtx server over HTTP (`virtx-core`'s protocol), as a library: the server its own binary
//! is, to the users of its config's provider, and what a server built on it gives in their place:
//!
//! - [`access::Identity`]: who a token is from, and the claims it carries: it authenticates. The
//!   binary's is its issuer, [`oidc::Oidc`].
//! - [`project::Membership`]: which project its sender names so, and their role in it: it
//!   authorizes. The binary's is [`project::TcpMembership`], each project kept in its directory
//!   with its members.
//! - [`access::Homes`]: what makes a project's home as it is first reached. The binary's is
//!   [`virtx_uvm::ProjectStore`], each home in the project's directory.
//! - [`serve::router`]'s `own`: routes beside the server's. The binary's say where to sign in,
//!   and serve the projects themselves ([`serve::projects`]); a server that keeps its projects
//!   elsewhere merges none of those.
//! - [`endpoints::Home::sandboxes`], [`park_unused`](endpoints::Home::park_unused) and
//!   [`park_all`](endpoints::Home::park_all), and [`access::Access::homes`]: a home's sandboxes,
//!   for routes of its own to count, or to park -- each one's machine stopped, the sandbox
//!   kept -- as it needs the host back.
//! - [`endpoints::Home::backed_by`]: where a project's data is kept beyond this machine, a
//!   [`virtx_uvm::StorageBackend`] given to each home its [`access::Homes`] makes, the home's own
//!   stores its cache. The binary's homes have none, and keep nothing beyond their stores.
//! - [`endpoints::Home::take_in`] and [`let_go`](endpoints::Home::let_go): a sandbox taken into a
//!   home whose record and disk were put there, and let go of with them left there, for moving a
//!   sandbox between servers, or backing it up and restoring it.
//! - [`virtx_uvm::set_admission`]: room for its machines, every project's together. A sandbox
//!   or a build there is none for is refused `503` `capacity`, nothing of it made. The binary
//!   sets none, and every machine boots.
//! - [`virtx_uvm::runs`]: each machine's coming up and going down, with when, in order, for it to
//!   meter or report machine time by.
//! - [`access::Access::through`]: what its image jobs connect to registries through. The
//!   binary's is its config's ([`config::Config::registries`]); without one, a server's image
//!   jobs reach registries on public addresses alone.
//! - [`access::Access::mirroring`]: Docker Hub's mirror, which its anonymous pulls of Docker
//!   Hub's images go to first. The binary's is its config's `registry_mirror`; without one,
//!   none.
//! - [`endpoints::Home::sharing`], or [`access::Access::sharing`] for every home: where a home's
//!   anonymous pulls are shared with other homes'. The binary's share nothing, unless its
//!   config's `share_public_images` shares them between its projects ([`shared::SharedPulls`]).

pub mod access;
pub mod config;
pub mod endpoints;
pub mod oidc;
pub mod project;
pub mod serve;
pub mod shared;
