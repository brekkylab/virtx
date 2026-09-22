use super::{Digest, Layer};

/// The manifest as a registry serves it: the config and layer descriptors an image is
/// made of, before any of it has been downloaded.
///
/// Re-exported rather than defined here. Nothing outside this module should name
/// `oci_client`'s own type, so that parsing the manifest ourselves later is a change to
/// this line and not to every caller.
pub use oci_client::manifest::OciImageManifest as ImageManifest;

/// One image. Identified by its manifest digest -- not by the reference that found it,
/// which many can share and none of which an image knows about itself.
#[derive(Clone, Debug)]
pub struct Image {
    pub manifest: Digest,

    /// Bottom first.
    pub layers: Vec<Layer>,

    pub config: ImageConfig,

    /// `os/arch`.
    pub platform: String,
}

/// The parts of the image config a boot needs.
#[derive(Clone, Debug, Default)]
pub struct ImageConfig {
    pub env: Vec<String>,

    pub entrypoint: Vec<String>,

    pub cmd: Vec<String>,

    pub workdir: Option<String>,

    pub user: Option<String>,
}
