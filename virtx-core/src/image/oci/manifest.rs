//! What the registry holds: manifests, the descriptors in them, and the digests that name both.

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256, Sha512};

use crate::{Error, image::is_digest};

/// Every manifest type asked for: OCI's and Docker's, each an image or an index of images.
pub(crate) const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, \
    application/vnd.oci.image.manifest.v1+json, \
    application/vnd.docker.distribution.manifest.list.v2+json, \
    application/vnd.docker.distribution.manifest.v2+json";

/// An image index, as an OCI image layout's `index.json` is.
pub(crate) const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";

/// The annotation naming a manifest in an OCI image layout, by its tag.
pub(crate) const REF_NAME: &str = "org.opencontainers.image.ref.name";

/// What points at a blob or manifest: its type, digest and size, and anything else it carries,
/// kept as it is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Descriptor {
    #[serde(rename = "mediaType")]
    pub(crate) media_type: String,
    pub(crate) digest: String,
    pub(crate) size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) annotations: Option<Map<String, Value>>,
    #[serde(flatten)]
    pub(crate) rest: Map<String, Value>,
}

/// What a manifest names: the manifests of an index, or the config and layers of an image.
#[derive(Deserialize)]
struct Names {
    #[serde(rename = "mediaType")]
    media_type: Option<String>,
    #[serde(default)]
    manifests: Vec<Descriptor>,
    config: Option<Descriptor>,
    #[serde(default)]
    layers: Vec<Descriptor>,
}

/// A manifest as it is stored: its bytes, which its digest is of, and its type.
#[derive(Clone, Debug)]
pub(crate) struct Manifest {
    pub(crate) media_type: String,
    pub(crate) digest: String,
    pub(crate) bytes: Bytes,
}

/// What a manifest names, each to be walked.
pub(crate) enum Child {
    Manifest(Descriptor),
    Blob(Descriptor),
}

impl Manifest {
    /// `bytes`, of type `media_type` or, without it, the type they say they are. Their digest
    /// is `expected`'s algorithm's, and must be `expected` if it is given.
    pub(crate) fn new(
        bytes: Bytes,
        media_type: Option<String>,
        expected: Option<&str>,
    ) -> Result<Manifest, Error> {
        let digest = match expected {
            Some(expected) => {
                let mut digester = Digester::new(expected)?;
                digester.update(&bytes);
                digester.check()?;
                expected.to_owned()
            }
            None => sha256(&bytes),
        };
        let names = names(&bytes)?;
        let media_type = media_type
            .filter(|media_type| !media_type.is_empty())
            .or(names.media_type)
            .ok_or_else(|| Error::Registry(format!("the manifest {digest} has no type")))?;
        Ok(Manifest {
            media_type,
            digest,
            bytes,
        })
    }

    /// Its descriptor, as an index names it.
    pub(crate) fn descriptor(&self) -> Descriptor {
        Descriptor {
            media_type: self.media_type.clone(),
            digest: self.digest.clone(),
            size: self.bytes.len() as u64,
            annotations: None,
            rest: Map::new(),
        }
    }

    /// What it names, manifests first. Each digest is checked to be one, since it becomes a
    /// path.
    pub(crate) fn children(&self) -> Result<Vec<Child>, Error> {
        let names = names(&self.bytes)?;
        let children: Vec<Child> = (names.manifests.into_iter().map(Child::Manifest))
            .chain(names.config.into_iter().map(Child::Blob))
            .chain(names.layers.into_iter().map(Child::Blob))
            .collect();
        for child in &children {
            let (Child::Manifest(descriptor) | Child::Blob(descriptor)) = child;
            if !is_digest(&descriptor.digest) {
                return Err(Error::Registry(format!(
                    "the manifest {} names {:?}, which is not a digest",
                    self.digest, descriptor.digest
                )));
            }
        }
        Ok(children)
    }
}

fn names(bytes: &[u8]) -> Result<Names, Error> {
    serde_json::from_slice(bytes).map_err(|e| Error::Registry(format!("not a manifest: {e}")))
}

/// `bytes`'s SHA-256 digest, as `sha256:hex`.
pub(crate) fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A digest being computed of what is read, to be checked against the one expected.
pub(crate) struct Digester {
    expected: String,
    hasher: Hasher,
}

enum Hasher {
    Sha256(Sha256),
    Sha512(Sha512),
}

impl Digester {
    /// A digester for `expected`, in its algorithm.
    pub(crate) fn new(expected: &str) -> Result<Digester, Error> {
        let hasher = match expected.split_once(':') {
            Some(("sha256", _)) if is_digest(expected) => Hasher::Sha256(Sha256::new()),
            Some(("sha512", _)) if is_digest(expected) => Hasher::Sha512(Sha512::new()),
            _ => return Err(Error::Registry(format!("{expected:?} is not a digest"))),
        };
        Ok(Digester {
            expected: expected.to_owned(),
            hasher,
        })
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        match &mut self.hasher {
            Hasher::Sha256(hasher) => hasher.update(bytes),
            Hasher::Sha512(hasher) => hasher.update(bytes),
        }
    }

    /// Whether what was read is what the digest expected says.
    pub(crate) fn check(self) -> Result<(), Error> {
        let (algorithm, hash) = match self.hasher {
            Hasher::Sha256(hasher) => ("sha256", hex(&hasher.finalize())),
            Hasher::Sha512(hasher) => ("sha512", hex(&hasher.finalize())),
        };
        let digest = format!("{algorithm}:{hash}");
        if digest == self.expected {
            Ok(())
        } else {
            Err(Error::Registry(format!(
                "{} was read as {digest}",
                self.expected
            )))
        }
    }
}

/// Where an OCI image layout keeps the blob `digest`: `blobs/algorithm/hex`.
pub(crate) fn blob_path(digest: &str) -> String {
    let (algorithm, hex) = digest.split_once(':').expect("a digest");
    format!("blobs/{algorithm}/{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_names_its_manifests_then_its_blobs() {
        let digest = |n: u8| format!("sha256:{}", format!("{n:x}").repeat(64));
        let descriptor = |n| serde_json::json!({"mediaType": "t", "digest": digest(n), "size": 1});
        let bytes = serde_json::to_vec(&serde_json::json!({
            "mediaType": OCI_INDEX,
            "manifests": [descriptor(1)],
            "config": descriptor(2),
            "layers": [descriptor(3), descriptor(4)],
        }))
        .unwrap();
        let manifest = Manifest::new(bytes.into(), None, None).unwrap();
        assert_eq!(manifest.media_type, OCI_INDEX);
        let children: Vec<_> = manifest
            .children()
            .unwrap()
            .into_iter()
            .map(|child| match child {
                Child::Manifest(d) => ("manifest", d.digest),
                Child::Blob(d) => ("blob", d.digest),
            })
            .collect();
        assert_eq!(
            children,
            [
                ("manifest", digest(1)),
                ("blob", digest(2)),
                ("blob", digest(3)),
                ("blob", digest(4)),
            ]
        );
    }

    #[test]
    fn a_manifest_naming_a_path_for_a_digest_is_refused() {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "mediaType": OCI_INDEX,
            "manifests": [{"mediaType": "t", "digest": "sha256:../../etc", "size": 1}],
        }))
        .unwrap();
        let manifest = Manifest::new(bytes.into(), None, None).unwrap();
        assert!(manifest.children().is_err());
    }

    #[test]
    fn a_digest_is_checked_in_its_own_algorithm() {
        let expected = sha256(b"abc");
        let mut digester = Digester::new(&expected).unwrap();
        digester.update(b"ab");
        digester.update(b"c");
        assert!(digester.check().is_ok());

        let mut digester = Digester::new(&expected).unwrap();
        digester.update(b"abd");
        assert!(digester.check().is_err());
        assert!(Digester::new("md5:0").is_err());
    }
}
