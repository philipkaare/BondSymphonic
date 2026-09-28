//! The long-lived Claude token: where it lives on disk, and how it is read,
//! written and removed.
//!
//! `claude setup-token` (§1 of the long-lived-token spec) mints a token that
//! does not expire the way an OAuth access token does, so agents can
//! authenticate as `CLAUDE_CODE_OAUTH_TOKEN` without the daemon having to
//! carry the login's refresh dance into every sandbox. This module is the
//! single place that touches the file it is kept in: nothing here ever logs
//! the token itself, only the path it lives at and whether the operation
//! succeeded.

use std::path::{Path, PathBuf};

/// The token file's name, relative to the daemon data root (`DataDirs.root`).
pub const TOKEN_FILE: &str = "claude-oauth-token";

/// Every long-lived token `claude setup-token` prints starts with this.
const PREFIX: &str = "sk-ant-oat01-";

/// The token file's path under the daemon data root.
pub fn token_path(root: &Path) -> PathBuf {
    root.join(TOKEN_FILE)
}

/// Whether `s` has the shape of a `claude setup-token` token: the fixed
/// prefix, then only `[A-Za-z0-9_-]`, with a total length between 40 and 512.
///
/// This is a shape check, not a validity check -- the daemon never asks
/// Anthropic whether a token is live. It exists so a truncated PTY read (a
/// wrapped line, a redraw caught mid-frame) is refused rather than stored as
/// if it were whole; see the long-lived-token spec and Task 3's capture code.
pub fn is_token_shaped(s: &str) -> bool {
    (40..=512).contains(&s.len())
        && s.starts_with(PREFIX)
        && s[PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The token at `path`, or `None` when there is nothing there, it is not a
/// plain file, or its contents (trimmed of surrounding whitespace) are not
/// token-shaped.
///
/// `symlink_metadata` rather than `metadata`: a symlink planted at this path
/// is never followed, so nothing outside the daemon data root is ever read as
/// if it were the token.
pub fn read(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    let body = std::fs::read_to_string(path).ok()?;
    let t = body.trim();
    is_token_shaped(t).then(|| t.to_owned())
}

/// Writes `token` to `path`, refusing anything that is not token-shaped
/// (`InvalidInput`) and refusing to write through a symlink found at `path`
/// (also `InvalidInput`).
///
/// The write itself goes through [`super::credentials::replace_private`]: a
/// temp file plus rename, mode 0600, so the token is never briefly readable
/// or briefly world-readable and a reader never sees a partial write.
pub fn write(path: &Path, token: &str) -> std::io::Result<()> {
    if !is_token_shaped(token) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a setup-token token",
        ));
    }
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "token path is a symlink",
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    super::credentials::replace_private(path, token.as_bytes())
}

/// Removes the token file, if there is one. `Ok(())` when it was already
/// absent: a logout with no token stored must not fail.
pub fn remove(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "sk-ant-oat01-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-AbCdEfGhIjKlAA";

    #[cfg(unix)]
    #[test]
    fn a_written_token_reads_back_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let p = token_path(dir.path());
        write(&p, T).unwrap();
        assert_eq!(read(&p).as_deref(), Some(T));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn surrounding_whitespace_is_not_part_of_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let p = token_path(dir.path());
        std::fs::write(&p, format!("  {T}\n")).unwrap();
        assert_eq!(read(&p).as_deref(), Some(T));
    }

    #[test]
    fn malformed_contents_read_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let p = token_path(dir.path());
        for bad in [
            "",
            "hello",
            "sk-ant-api03-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            "sk-ant-oat01-short",
            &format!("{T} extra"),
        ] {
            std::fs::write(&p, bad).unwrap();
            assert_eq!(read(&p), None, "{bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_neither_read_through_nor_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, T).unwrap();
        let p = token_path(dir.path());
        std::os::unix::fs::symlink(&target, &p).unwrap();
        assert_eq!(read(&p), None);
        assert!(write(&p, T).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), T);
    }

    #[test]
    fn a_string_that_is_not_a_token_is_refused_by_write() {
        let dir = tempfile::tempdir().unwrap();
        let p = token_path(dir.path());
        assert_eq!(
            write(&p, "sk-ant-oat01-sh").unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(!p.exists());
    }

    #[test]
    fn removing_an_absent_token_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let p = token_path(dir.path());
        remove(&p).unwrap();
        write(&p, T).unwrap();
        remove(&p).unwrap();
        assert!(!p.exists());
    }
}
