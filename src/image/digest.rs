use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use sha2::{Digest as _, Sha256};

use super::Error;

/// A content digest, `sha256:e3b0c442…`. Kept split so paths can avoid the colon,
/// which NTFS refuses.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest {
    algorithm: String,
    hex: String,
}

impl Digest {
    /// The SHA-256 of `bytes`.
    pub fn of(bytes: impl AsRef<[u8]>) -> Digest {
        Digest {
            algorithm: "sha256".to_string(),
            hex: hex::encode(Sha256::digest(bytes)),
        }
    }

    /// Read a digest, checking the algorithm and the hex -- a digest is used as a file
    /// name, so one that is not what it claims would name a real-looking blob.
    pub fn parse(text: impl AsRef<str>) -> Result<Digest, Error> {
        let text = text.as_ref();
        let (algorithm, hex) = text
            .split_once(':')
            .ok_or_else(|| Error::Digest(format!("{text} has no `algorithm:` prefix")))?;

        let width = match algorithm {
            "sha256" => 64,
            "sha512" => 128,
            other => return Err(Error::Digest(format!("{other} is not a digest algorithm"))),
        };
        if hex.len() != width {
            return Err(Error::Digest(format!(
                "a {algorithm} digest is {width} hex characters, and {text} has {}",
                hex.len()
            )));
        }
        if !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Digest(format!(
                "{text} has something other than lowercase hex after the colon"
            )));
        }

        Ok(Digest {
            algorithm: algorithm.to_string(),
            hex: hex.to_string(),
        })
    }

    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    pub fn hex(&self) -> &str {
        &self.hex
    }

    /// `sha256/e3b0c442…`, as the OCI image-layout spec lays out `blobs/`.
    pub fn path(&self) -> PathBuf {
        PathBuf::from(&self.algorithm).join(&self.hex)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algorithm, self.hex)
    }
}

impl FromStr for Digest {
    type Err = Error;

    fn from_str(text: &str) -> Result<Digest, Error> {
        Digest::parse(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_round_trips_through_its_text() {
        let text = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let digest = Digest::parse(text).unwrap();
        assert_eq!(digest.to_string(), text);
        assert_eq!(digest.algorithm(), "sha256");
    }

    #[test]
    fn hashing_agrees_with_the_known_answer() {
        assert_eq!(
            Digest::of(b"").to_string(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_path_is_a_directory_and_a_name() {
        let digest = Digest::of(b"");
        assert_eq!(digest.path(), PathBuf::from("sha256").join(digest.hex()));
    }

    #[test]
    fn what_is_not_a_digest_is_refused() {
        for text in [
            "e3b0c442",
            "md5:e3b0c442",
            "sha256:abc",
            "sha256:E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855",
        ] {
            assert!(Digest::parse(text).is_err(), "{text} was accepted");
        }
    }
}
