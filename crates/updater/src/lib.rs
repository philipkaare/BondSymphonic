//! Self-update for BondSymphonic, from the project's GitHub releases.
//!
//! Every release carries `latest.json` (see [`manifest`]) and its minisign
//! signature. GitHub serves the newest release's copy at a fixed URL, so a
//! check is two small GETs and no API calls -- no token, no rate limit. The
//! manifest is verified against the key compiled into this crate
//! (`update-key.pub`) before a single field of it is believed, and a package
//! is only kept if its size and SHA-256 match what the manifest signed.
//!
//! Nothing here knows about Qt; the IDE's `UpdateController` drives it from a
//! background thread.

pub mod fetch;
pub mod install;
pub mod manifest;
pub mod version;

pub use fetch::FetchError;
pub use install::{InstallError, InstallKind, Prepared};
pub use manifest::{ManifestError, Release};
pub use version::Version;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// Where releases are published.
pub const REPOSITORY: &str = "philipkaare/BondSymphonic";

/// The release-signing public key (minisign `.pub` file contents).
pub const PUBLIC_KEY: &str = include_str!("../update-key.pub");

/// The manifest's file name on every release.
pub const MANIFEST_FILE: &str = "latest.json";

/// The manifest names more than this only if something is badly wrong.
const MANIFEST_LIMIT: u64 = 64 * 1024;
const SIGNATURE_LIMIT: u64 = 4 * 1024;

/// This build's key into a manifest's `assets`, or `None` for a platform no
/// release is built for.
pub fn current_platform() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some("windows-x86_64")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("macos-aarch64")
    } else {
        None
    }
}

/// Where to look and what to trust.
#[derive(Debug, Clone)]
pub struct Source {
    pub manifest_url: String,
    pub signature_url: String,
    /// `https://github.com/<repo>/releases/download`: every asset URL in a
    /// manifest must sit under `<this>/v<version>/`.
    pub releases_url: String,
    pub public_key: String,
}

impl Source {
    /// The newest release of `repo` on GitHub, trusted with [`PUBLIC_KEY`].
    pub fn github(repo: &str) -> Self {
        let manifest_url =
            format!("https://github.com/{repo}/releases/latest/download/{MANIFEST_FILE}");
        Self {
            signature_url: format!("{manifest_url}.minisig"),
            manifest_url,
            releases_url: format!("https://github.com/{repo}/releases/download"),
            public_key: PUBLIC_KEY.to_owned(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("{0}")]
    Unsupported(String),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Install(#[from] InstallError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// The newest release is this build's version or older.
    UpToDate {
        latest: Version,
    },
    Available(Release),
}

pub struct Updater {
    client: fetch::Client,
    source: Source,
    current: Version,
    platform: &'static str,
}

impl Updater {
    /// An updater for a build of version `current`. Fails on a platform no
    /// release is built for, or a build with no update key.
    pub fn new(current: &str, source: Source) -> Result<Self, UpdateError> {
        let platform = current_platform().ok_or_else(|| {
            UpdateError::Unsupported("no releases are built for this platform".into())
        })?;
        if !manifest::has_key(&source.public_key) {
            return Err(ManifestError::NoKey.into());
        }
        let current = current
            .parse::<Version>()
            .map_err(|e| UpdateError::Unsupported(format!("this build's version: {e}")))?;
        Ok(Self {
            client: fetch::Client::new(&format!("BondSymphonic/{current} ({platform})")),
            source,
            current,
            platform,
        })
    }

    pub fn current(&self) -> Version {
        self.current
    }

    /// Fetches and verifies the newest release's manifest.
    pub fn check(&self) -> Result<Check, UpdateError> {
        let json = self
            .client
            .get_small(&self.source.manifest_url, MANIFEST_LIMIT)?;
        let sig = self
            .client
            .get_small(&self.source.signature_url, SIGNATURE_LIMIT)?;
        let sig =
            String::from_utf8(sig).map_err(|_| ManifestError::BadSignature("not UTF-8".into()))?;
        let manifest = manifest::verify(&json, &sig, &self.source.public_key)?;
        let release = manifest.release_for(self.platform, &self.source.releases_url)?;
        tracing::info!(current = %self.current, latest = %release.version, "update check");
        Ok(decide(self.current, release))
    }

    /// Downloads `release` into `dir`, verified, and returns the file's path.
    /// A verified copy already there from an earlier run is reused.
    pub fn download(
        &self,
        release: &Release,
        dir: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<PathBuf, UpdateError> {
        std::fs::create_dir_all(dir).map_err(FetchError::from)?;
        let dest = dir.join(release.file_name());
        if file_matches(&dest, &release.asset) {
            progress(release.asset.size, release.asset.size);
            return Ok(dest);
        }
        let _ = std::fs::remove_file(&dest);
        self.client
            .download(&release.asset, &dest, cancel, progress)?;
        Ok(dest)
    }
}

/// Only a strictly newer release is offered: a manifest naming an older one
/// (a replayed old signature, a release rolled back) is never a downgrade.
pub fn decide(current: Version, release: Release) -> Check {
    if release.version > current {
        Check::Available(release)
    } else {
        Check::UpToDate {
            latest: release.version,
        }
    }
}

/// Whether `path` holds exactly the bytes `asset` describes.
pub fn file_matches(path: &Path, asset: &manifest::Asset) -> bool {
    use sha2::Digest;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() != asset.size {
        return false;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut hasher = sha2::Sha256::new();
    if std::io::copy(&mut file, &mut hasher).is_err() {
        return false;
    }
    fetch::hex(&hasher.finalize()) == asset.sha256.to_ascii_lowercase()
}

/// Removes everything in `dir` except the paths in `keep`: earlier downloads,
/// half downloads, extracted bundles. Errors are ignored; this is housekeeping.
pub fn prune(dir: &Path, keep: &[&Path]) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if keep.contains(&path.as_path()) {
            continue;
        }
        let _ = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifest::{Asset, Release};

    fn release(v: &str) -> Release {
        Release {
            version: v.parse().unwrap(),
            commit: String::new(),
            notes_url: String::new(),
            asset: Asset {
                url: "https://x/y".into(),
                sha256: "00".repeat(32),
                size: 1,
            },
        }
    }

    #[test]
    fn only_a_newer_release_is_offered() {
        let cur: Version = "0.1.5".parse().unwrap();
        assert!(matches!(decide(cur, release("0.1.6")), Check::Available(_)));
        assert!(matches!(
            decide(cur, release("0.1.5")),
            Check::UpToDate { .. }
        ));
        assert!(matches!(
            decide(cur, release("0.1.4")),
            Check::UpToDate { .. }
        ));
    }

    #[test]
    fn the_github_source_uses_the_latest_release_redirect() {
        let s = Source::github("o/r");
        assert_eq!(
            s.manifest_url,
            "https://github.com/o/r/releases/latest/download/latest.json"
        );
        assert_eq!(
            s.signature_url,
            "https://github.com/o/r/releases/latest/download/latest.json.minisig"
        );
        assert_eq!(s.releases_url, "https://github.com/o/r/releases/download");
    }

    #[test]
    fn file_matches_checks_size_and_hash() {
        use sha2::Digest;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, b"hello").unwrap();
        let good = Asset {
            url: String::new(),
            sha256: fetch::hex(&sha2::Sha256::digest(b"hello")),
            size: 5,
        };
        assert!(file_matches(&path, &good));
        assert!(!file_matches(
            &path,
            &Asset {
                size: 4,
                ..good.clone()
            }
        ));
        assert!(!file_matches(
            &path,
            &Asset {
                sha256: "00".repeat(32),
                ..good.clone()
            }
        ));
        assert!(!file_matches(&dir.path().join("missing"), &good));
    }

    #[test]
    fn prune_keeps_only_the_named_file() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("keep");
        std::fs::write(&keep, b"").unwrap();
        std::fs::write(dir.path().join("old.exe"), b"").unwrap();
        std::fs::create_dir(dir.path().join("extracted")).unwrap();
        prune(dir.path(), &[&keep]);
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1);
        assert!(keep.exists());
    }

    #[test]
    fn a_build_without_a_key_cannot_update() {
        let mut source = Source::github("o/r");
        source.public_key = "# placeholder\n".into();
        assert!(Updater::new("0.1.0", source).is_err());
    }
}
