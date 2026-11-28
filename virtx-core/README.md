# virtx-core

The virtx client behind a C ABI: the types every request takes and answers, the wire between a client and a server, and `libvirtx_core`, the library the virtx packages for Rust, Python and Node load.

A Rust program takes [`virtx`](https://crates.io/crates/virtx), which links this crate's client in. This crate is for what is built under it:

- **`client`**: the client over the wire, HTTP over TCP, `https` included, or HTTP/2 over a child's stdio. `ffi` and `cli` take it.
- **`ffi`** (default): the C ABI over that client, declared by `include/virtx.h`, which the repository's `virtx-cabi` builds as `libvirtx_core.{so,dylib}` (`virtx_core.dll` on Windows).
- **`cli`** (default): the `virtx` command `libvirtx_core` carries.
- **`server`**: the server end of the wire, which the virtx servers are built on.

With `default-features = false` it is the types alone, for a crate that reaches the library through its C ABI.

The rest of virtx is [on GitHub](https://github.com/brekkylab/virtx).
