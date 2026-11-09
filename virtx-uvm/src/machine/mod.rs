//! A machine: a root filesystem booted with a writable layer over it, running the guest, which
//! runs the commands it is sent.
//!
//! A machine is a process of its own. Booting one makes the process that does it into the
//! machine, which does not come back: the process ends when the guest does. So
//! [`Machine::start`] spawns the command it is given, with the machine's boot args as its last
//! argument, to be the machine -- usually the binary it is in, again, under a subcommand of the
//! caller's. On macOS that is also what the hypervisor entitlement needs, since it is read from
//! the signature at `exec`. That binary's `main` calls [`enter`] before anything else when run
//! so, and [`enter`] builds the machine and becomes it (see `boot`).
//!
//! A machine stays up once it has booted, and runs the commands it is sent side by side, each a
//! process of the guest's under its pid there ([`Machine::exec`]), until it is stopped
//! ([`Machine::stop`]), or dropped, which kills it. The host and the process talk over two pipes
//! (see [`wire`]): `ctrlin`, which the host sends commands and signals through, and `ctrlout`
//! ([`Machine::recv`]), which the process says everything on -- that the guest is up and where
//! its ports are published, each process's id as it starts, its output and how it ended under
//! that id, and the process's log. Two more, `ioin` and `ioout`, run past the process to the
//! agent, which reads and writes the guest's files over them ([`Machine::io`]).

mod boot;
mod error;
#[cfg(feature = "gpu")]
mod gpu;
#[allow(clippy::module_inception)]
mod machine;
mod net;
mod ports;
pub mod wire;

pub use error::{Error, Result};
pub use machine::*;
pub use net::Network;
pub use ports::{Ports, Publication};
