//! What an image is, wherever it is kept: its layers, in order, and what it says about
//! running.

/// What an image says about running it, as an image config says it -- re-exported so that
/// nothing outside has to name `oci_spec` to hold one.
pub use oci_spec::image::Config;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::filesystem::Digest;

/// One image. Identified by its manifest digest, not by any reference that found it: many
/// references can name one image, and an image knows none of them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Image {
    pub manifest: Digest,

    /// Each layer's `diff_id`, bottom first.
    pub layers: Vec<Digest>,

    /// What the image config says about running it, as the config says it.
    pub config: Option<Config>,

    /// `os/arch`.
    pub platform: String,
}

impl Image {
    /// The [`chain_id`] of the image's layers.
    pub fn chain_id(&self) -> Option<Digest> {
        chain_id(&self.layers)
    }
}

/// The ChainID of `layers`, bottom first, as the image spec defines it: a lone layer's is
/// its `diff_id`, and each layer over a chain makes the SHA-256 of the chain's ChainID, a
/// space, and its own `diff_id`.
///
/// What names the filesystem the layers make, and so is shared by every stack of the same
/// layers in the same order, whether an image or not. `None` for no layers, which make no
/// filesystem.
pub fn chain_id(layers: &[Digest]) -> Option<Digest> {
    let (bottom, above) = layers.split_first()?;
    Some(above.iter().fold(bottom.clone(), |chain, layer| {
        format!("sha256:{:x}", Sha256::digest(format!("{chain} {layer}")))
            .parse()
            .expect("a SHA-256 spelled as a digest is one")
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(hex: char) -> Digest {
        format!("sha256:{}", hex.to_string().repeat(64))
            .parse()
            .unwrap()
    }

    fn image(layers: Vec<Digest>) -> Image {
        Image {
            manifest: digest('f'),
            layers,
            config: None,
            platform: "linux/arm64".to_string(),
        }
    }

    /// The image spec's own definition, spelled out: a lone layer is its own chain, and
    /// each one over it hashes the chain so far with itself.
    #[test]
    fn a_chain_id_is_the_image_specs() {
        let (a, b) = (digest('a'), digest('b'));
        assert_eq!(image(vec![]).chain_id(), None);
        assert_eq!(image(vec![a.clone()]).chain_id(), Some(a.clone()));

        let expected: Digest = format!("sha256:{:x}", Sha256::digest(format!("{a} {b}")))
            .parse()
            .unwrap();
        assert_eq!(
            image(vec![a.clone(), b.clone()]).chain_id(),
            Some(expected.clone())
        );
        assert_ne!(
            image(vec![b, a]).chain_id(),
            Some(expected),
            "the order does not count"
        );
    }
}
