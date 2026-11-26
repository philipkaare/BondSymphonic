//! HTTP: the small signed files, and the package download.

use crate::manifest::Asset;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("{url}: {source}")]
    Http {
        url: String,
        #[source]
        source: ureq::Error,
    },
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("the download is {actual} bytes, but the release says {expected}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("the download's SHA-256 does not match the signed release manifest")]
    HashMismatch,
    #[error("cancelled")]
    Cancelled,
}

pub struct Client {
    agent: ureq::Agent,
}

impl Client {
    /// `user_agent` names the build asking; GitHub refuses requests with none.
    pub fn new(user_agent: &str) -> Self {
        let tls = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        let agent = ureq::Agent::config_builder()
            .tls_config(tls)
            .https_only(true)
            .user_agent(user_agent)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            // No overall cap on a body: a package on a slow line takes as long
            // as it takes. A stalled one is caught per read instead.
            .timeout_per_read(Some(Duration::from_secs(60)))
            .build()
            .new_agent();
        Self { agent }
    }

    /// The body at `url`, refused past `limit` bytes.
    pub fn get_small(&self, url: &str, limit: u64) -> Result<Vec<u8>, FetchError> {
        let http = |source| FetchError::Http {
            url: url.to_owned(),
            source,
        };
        let mut response = self.agent.get(url).call().map_err(http)?;
        response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(http)
    }

    /// Downloads `asset` to `dest`, verified.
    ///
    /// The bytes go to `<dest>.part` and are hashed as they arrive; only a file
    /// of exactly the signed size and hash is renamed to `dest`, so `dest`
    /// existing means it is the genuine package. `progress` hears (bytes so
    /// far, total) after every chunk, and `cancel` is checked as often.
    pub fn download(
        &self,
        asset: &Asset,
        dest: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), FetchError> {
        let part = part_path(dest);
        let result = self.download_to(asset, &part, cancel, progress);
        if result.is_err() {
            let _ = std::fs::remove_file(&part);
            return result;
        }
        std::fs::rename(&part, dest)?;
        Ok(())
    }

    fn download_to(
        &self,
        asset: &Asset,
        part: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), FetchError> {
        let http = |source| FetchError::Http {
            url: asset.url.clone(),
            source,
        };
        let response = self.agent.get(&asset.url).call().map_err(http)?;
        // One byte over the signed size is enough to know it is wrong; the
        // limit stops a server that never ends the body.
        let reader = response
            .into_body()
            .into_with_config()
            .limit(asset.size + 1)
            .reader();
        let file = std::fs::File::create(part)?;
        copy_verified(reader, file, asset, cancel, progress)
    }
}

/// `<dest>.part`.
pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// Copies `reader` into `out`, checking the byte count and SHA-256 against
/// `asset` at the end. Split from the HTTP half so it is testable without one.
pub(crate) fn copy_verified(
    mut reader: impl Read,
    out: std::fs::File,
    asset: &Asset,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<(), FetchError> {
    let mut out = std::io::BufWriter::new(out);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total: u64 = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(FetchError::Cancelled);
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        total += n as u64;
        if total > asset.size {
            return Err(FetchError::SizeMismatch {
                expected: asset.size,
                actual: total,
            });
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        progress(total, asset.size);
    }
    if total != asset.size {
        return Err(FetchError::SizeMismatch {
            expected: asset.size,
            actual: total,
        });
    }
    if hex(&hasher.finalize()) != asset.sha256.to_ascii_lowercase() {
        return Err(FetchError::HashMismatch);
    }
    let file = out.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    Ok(())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset_for(data: &[u8]) -> Asset {
        Asset {
            url: String::new(),
            sha256: hex(&Sha256::digest(data)),
            size: data.len() as u64,
        }
    }

    fn run(data: &[u8], asset: &Asset) -> Result<(), FetchError> {
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(dir.path().join("x")).unwrap();
        copy_verified(data, file, asset, &AtomicBool::new(false), &mut |_, _| {})
    }

    #[test]
    fn accepts_exactly_the_signed_bytes() {
        let data = vec![7u8; 600_000];
        let mut seen = 0;
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(dir.path().join("x")).unwrap();
        copy_verified(
            &data[..],
            file,
            &asset_for(&data),
            &AtomicBool::new(false),
            &mut |n, _| seen = n,
        )
        .unwrap();
        assert_eq!(seen, 600_000);
    }

    #[test]
    fn refuses_a_different_body_of_the_same_size() {
        let asset = asset_for(b"genuine");
        assert!(matches!(
            run(b"forgery", &asset),
            Err(FetchError::HashMismatch)
        ));
    }

    #[test]
    fn refuses_short_and_long_bodies() {
        let asset = asset_for(b"genuine");
        assert!(matches!(
            run(b"genuin", &asset),
            Err(FetchError::SizeMismatch { .. })
        ));
        assert!(matches!(
            run(b"genuine!", &asset),
            Err(FetchError::SizeMismatch { .. })
        ));
    }

    #[test]
    fn stops_when_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(dir.path().join("x")).unwrap();
        let r = copy_verified(
            &b"data"[..],
            file,
            &asset_for(b"data"),
            &AtomicBool::new(true),
            &mut |_, _| {},
        );
        assert!(matches!(r, Err(FetchError::Cancelled)));
    }

    #[test]
    fn the_part_file_sits_beside_the_destination() {
        assert_eq!(
            part_path(Path::new("/a/b/setup.exe")),
            PathBuf::from("/a/b/setup.exe.part")
        );
    }
}
