//! Release versions: exactly `major.minor.patch`, compared numerically.
//!
//! The release workflow only ever produces plain triples (`0.1.<n>`), so
//! anything else -- a pre-release suffix, build metadata, a leading `v` -- is
//! refused rather than half-understood. A manifest that names a version this
//! parser rejects is a manifest the updater ignores.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a version of the form major.minor.patch")]
pub struct BadVersion(pub String);

impl FromStr for Version {
    type Err = BadVersion;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || BadVersion(s.to_owned());
        let mut parts = s.split('.');
        let mut next = || -> Result<u64, BadVersion> {
            let part = parts.next().ok_or_else(bad)?;
            // `u64::from_str` accepts a leading `+`; a version does not.
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            part.parse().map_err(|_| bad())
        };
        let version = Version {
            major: next()?,
            minor: next()?,
            patch: next()?,
        };
        if parts.next().is_some() {
            return Err(bad());
        }
        Ok(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn parses_and_prints_triples() {
        assert_eq!(
            v("0.1.12"),
            Version {
                major: 0,
                minor: 1,
                patch: 12
            }
        );
        assert_eq!(v("10.0.3").to_string(), "10.0.3");
    }

    #[test]
    fn compares_numerically_not_lexically() {
        assert!(v("0.1.10") > v("0.1.9"));
        assert!(v("0.2.0") > v("0.1.99"));
        assert!(v("1.0.0") > v("0.99.99"));
    }

    #[test]
    fn refuses_anything_but_a_plain_triple() {
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "v1.2.3",
            "1.2.3-rc1",
            "1.2.3+b",
            "1..3",
            "+1.2.3",
            " 1.2.3",
            "1.2.x",
        ] {
            assert!(bad.parse::<Version>().is_err(), "{bad:?} was accepted");
        }
    }
}
