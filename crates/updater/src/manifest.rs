//! The release manifest: `latest.json`, attached to every GitHub release and
//! signed with the project's minisign key (`latest.json.minisig`).
//!
//! The manifest is the only thing the signature covers, so everything else is
//! pinned through it: the version the release claims, and for each platform
//! the download's URL, size and SHA-256. A download whose hash does not match
//! the signed manifest is discarded, whoever served it.

use crate::version::Version;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The largest download the updater will accept. The packages are a few
/// hundred megabytes at most; a manifest naming more is wrong, and the cap is
/// what keeps a wrong one from filling the disk.
pub const MAX_ASSET_SIZE: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub notes_url: String,
    /// Keyed by [`crate::platform::current`]: `windows-x86_64`, `macos-aarch64`.
    pub assets: BTreeMap<String, Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

/// One platform's view of a verified manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub commit: String,
    pub notes_url: String,
    pub asset: Asset,
}

impl Release {
    /// The asset's file name: the last segment of its URL. Checked to be a
    /// plain name by [`Manifest::release_for`], so it is safe to join onto a
    /// directory.
    pub fn file_name(&self) -> &str {
        self.asset.url.rsplit('/').next().unwrap_or_default()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("this build has no update key, so no update can be verified")]
    NoKey,
    #[error("the update key compiled into this build is malformed: {0}")]
    BadKey(String),
    #[error("the release signature is malformed: {0}")]
    BadSignature(String),
    #[error("the release manifest is not signed by the BondSymphonic release key")]
    SignatureMismatch,
    #[error("the release manifest is malformed: {0}")]
    Malformed(String),
    #[error("the latest release has no download for this platform ({0})")]
    NoAssetForPlatform(String),
}

/// The key text, or [`ManifestError::NoKey`] for the placeholder the
/// repository ships until a real key is generated: a file with no key line.
fn decode_key(public_key: &str) -> Result<minisign_verify::PublicKey, ManifestError> {
    let lines: Vec<&str> = public_key
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let Some(key_line) = lines.iter().find(|l| !l.starts_with("untrusted comment:")) else {
        return Err(ManifestError::NoKey);
    };
    minisign_verify::PublicKey::from_base64(key_line)
        .map_err(|e| ManifestError::BadKey(e.to_string()))
}

/// Whether `public_key` holds a key at all, as opposed to the placeholder.
pub fn has_key(public_key: &str) -> bool {
    !matches!(decode_key(public_key), Err(ManifestError::NoKey))
}

/// Checks `signature` over `json` with `public_key`, then parses it.
///
/// Legacy (non-prehashed) signatures are refused: the release workflow signs
/// with a current minisign, which prehashes, and accepting both would only
/// widen what a forged file could look like.
pub fn verify(json: &[u8], signature: &str, public_key: &str) -> Result<Manifest, ManifestError> {
    let key = decode_key(public_key)?;
    let sig = minisign_verify::Signature::decode(signature)
        .map_err(|e| ManifestError::BadSignature(e.to_string()))?;
    key.verify(json, &sig, false)
        .map_err(|_| ManifestError::SignatureMismatch)?;
    serde_json::from_slice(json).map_err(|e| ManifestError::Malformed(e.to_string()))
}

impl Manifest {
    /// The release this manifest describes for `platform`, with every field
    /// checked: a version the updater understands, an asset served from this
    /// repository's own release of that version, a plain file name, a real
    /// SHA-256 and a believable size.
    ///
    /// `releases_url` is `https://github.com/<owner>/<repo>/releases/download`.
    /// The URL check is defence in depth -- the hash would catch a swapped file
    /// anyway -- but it also keeps a correctly signed manifest from sending the
    /// updater to fetch from a host nobody chose.
    pub fn release_for(
        &self,
        platform: &str,
        releases_url: &str,
    ) -> Result<Release, ManifestError> {
        let bad = ManifestError::Malformed;
        let version = self
            .version
            .parse::<Version>()
            .map_err(|e| bad(format!("{e}")))?;
        let asset = self
            .assets
            .get(platform)
            .ok_or_else(|| ManifestError::NoAssetForPlatform(platform.to_owned()))?;
        let prefix = format!("{}/v{version}/", releases_url.trim_end_matches('/'));
        let name = asset
            .url
            .strip_prefix(&prefix)
            .ok_or_else(|| bad(format!("{} is not under {prefix}", asset.url)))?;
        let plain = !name.is_empty()
            && name != "."
            && name != ".."
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
        if !plain {
            return Err(bad(format!("{name:?} is not a plain file name")));
        }
        if asset.sha256.len() != 64 || !asset.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad(format!("{:?} is not a SHA-256", asset.sha256)));
        }
        if asset.size == 0 || asset.size > MAX_ASSET_SIZE {
            return Err(bad(format!(
                "{} bytes is not a believable package size",
                asset.size
            )));
        }
        Ok(Release {
            version,
            commit: self.commit.clone(),
            notes_url: self.notes_url.clone(),
            asset: Asset {
                sha256: asset.sha256.to_ascii_lowercase(),
                ..asset.clone()
            },
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const RELEASES: &str = "https://github.com/o/r/releases/download";

    /// A fresh key pair and a function that signs with it, as minisign would.
    pub fn signer() -> (String, impl Fn(&[u8]) -> String) {
        let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let pk = kp.pk.to_box().unwrap().into_string();
        let sign = move |data: &[u8]| {
            minisign::sign(
                Some(&kp.pk),
                &kp.sk,
                std::io::Cursor::new(data),
                Some("test"),
                None,
            )
            .unwrap()
            .into_string()
        };
        (pk, sign)
    }

    pub fn manifest_json(version: &str, url: &str, sha: &str, size: u64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": version,
            "commit": "abc",
            "notes_url": "https://example/notes",
            "assets": { "windows-x86_64": { "url": url, "sha256": sha, "size": size } }
        }))
        .unwrap()
    }

    fn good_json() -> Vec<u8> {
        manifest_json(
            "0.1.7",
            &format!("{RELEASES}/v0.1.7/BondSymphonic-0.1.7-windows-x64-setup.exe"),
            &"ab".repeat(32),
            1234,
        )
    }

    #[test]
    fn accepts_a_manifest_signed_with_the_key() {
        let (pk, sign) = signer();
        let json = good_json();
        let m = verify(&json, &sign(&json), &pk).unwrap();
        let r = m.release_for("windows-x86_64", RELEASES).unwrap();
        assert_eq!(r.version.to_string(), "0.1.7");
        assert_eq!(r.file_name(), "BondSymphonic-0.1.7-windows-x64-setup.exe");
    }

    #[test]
    fn refuses_a_tampered_manifest() {
        let (pk, sign) = signer();
        let json = good_json();
        let sig = sign(&json);
        let mut tampered = json.clone();
        let at = tampered.iter().position(|&b| b == b'7').unwrap();
        tampered[at] = b'8';
        assert!(matches!(
            verify(&tampered, &sig, &pk),
            Err(ManifestError::SignatureMismatch)
        ));
    }

    #[test]
    fn refuses_a_manifest_signed_with_another_key() {
        let (pk, _) = signer();
        let (_, other_sign) = signer();
        let json = good_json();
        assert!(verify(&json, &other_sign(&json), &pk).is_err());
    }

    #[test]
    fn the_placeholder_key_file_is_no_key() {
        let placeholder = include_str!("../update-key.pub");
        if placeholder.contains("untrusted comment:") {
            // A real key has been committed; nothing to check here.
            return;
        }
        assert!(!has_key(placeholder));
        assert!(matches!(
            verify(b"{}", "", placeholder),
            Err(ManifestError::NoKey)
        ));
    }

    #[test]
    fn refuses_assets_outside_the_versions_own_release() {
        let m = |url: &str| Manifest {
            version: "0.1.7".into(),
            commit: String::new(),
            notes_url: String::new(),
            assets: [(
                "windows-x86_64".to_owned(),
                Asset {
                    url: url.into(),
                    sha256: "ab".repeat(32),
                    size: 10,
                },
            )]
            .into(),
        };
        for url in [
            "https://evil.example/v0.1.7/x.exe",
            &format!("{RELEASES}/v0.1.6/x.exe"),
            &format!("{RELEASES}/v0.1.7/../x.exe"),
            &format!("{RELEASES}/v0.1.7/sub/x.exe"),
            &format!("{RELEASES}/v0.1.7/"),
        ] {
            assert!(
                m(url).release_for("windows-x86_64", RELEASES).is_err(),
                "{url} was accepted"
            );
        }
        assert!(matches!(
            m(&format!("{RELEASES}/v0.1.7/x.exe")).release_for("macos-aarch64", RELEASES),
            Err(ManifestError::NoAssetForPlatform(_))
        ));
    }

    #[test]
    fn refuses_bad_hashes_and_sizes() {
        let url = format!("{RELEASES}/v0.1.7/x.exe");
        for (sha, size) in [
            ("ab".repeat(31), 10),
            ("zz".repeat(32), 10),
            ("ab".repeat(32), 0),
            ("ab".repeat(32), MAX_ASSET_SIZE + 1),
        ] {
            let json = manifest_json("0.1.7", &url, &sha, size);
            let m: Manifest = serde_json::from_slice(&json).unwrap();
            assert!(m.release_for("windows-x86_64", RELEASES).is_err());
        }
    }
}
