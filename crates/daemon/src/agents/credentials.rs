//! Copying the daemon user's Claude Code login into a workspace home.
//!
//! An agent runs inside a sandbox whose `HOME` is a private per-workspace
//! directory, so it cannot see the daemon user's `~/.claude`. Without help it
//! would start logged out and exit immediately. Rather than share the real home
//! into the sandbox — which would give every agent write access to the user's
//! whole Claude configuration, and to whatever else lives there — the few files
//! that carry the login are copied in.
//!
//! Copying happens both when a workspace is created and every time an agent
//! starts, so a login performed after the workspace existed reaches it without
//! the user having to recreate anything. It is a mirror rather than a top-up:
//! a file the daemon user no longer has is removed from the workspace home too,
//! so `claude auth logout` reaches every existing workspace at the next start
//! instead of leaving each one its own working copy of the session that was
//! just put away.
//!
//! `.claude.json` is the one file that is written rather than copied: the
//! workspace's worktree is added to it as a trusted project (§8.3), because
//! Claude Code keeps that consent per project directory and a sandbox home has
//! never seen this one.
//!
//! The login also travels *back*. An OAuth access token expires within hours,
//! and when it does the CLI inside the sandbox refreshes it — and the refresh
//! rotates the refresh token, so the one the daemon user's own file still holds
//! is dead from that moment. Nothing told the host: `ws_be2db101`'s copy was
//! rewritten by a refresh on 2026-09-14, every later start seeded that dead
//! refresh token into a fresh sandbox, and each new agent answered "OAuth
//! session expired and could not be refreshed" while `claude auth status` on
//! the host, which only reads the file, said "logged in". So a workspace copy
//! that is newer and still a working login is copied back to the host: see
//! [`write_back_login_from`] for what it must be, and for the one way this
//! could do harm.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// The files copied out of the daemon user's home, relative to both homes, with
/// whether the file is a secret.
///
/// `.credentials.json` holds the OAuth tokens and `.claude.json` the account
/// and per-project state, so both are created mode 0600 and never exist wider
/// than that. `settings.json` is configuration and keeps the default mode.
const FILES: [(&str, bool); 3] = [
    (".claude/settings.json", false),
    (".claude/.credentials.json", true),
    (CLAUDE_JSON, true),
];

/// The file Claude Code keeps its per-project state in, including which project
/// directories the user has accepted the trust dialog for.
const CLAUDE_JSON: &str = ".claude.json";

/// The daemon user's home, or `None` when the platform will not name one.
fn daemon_home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// Removes whatever is at `path` without ever following it.
///
/// A symlink is unlinked — the link itself goes, its target is untouched — and a
/// real file or directory is deleted. `Ok(())` when there was nothing there.
pub(crate) fn remove_any(path: &Path) -> std::io::Result<()> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if md.is_symlink() {
        // Windows makes a symlink to a directory a directory entry, and
        // `remove_file` refuses it; `remove_dir` unlinks the link and leaves the
        // target alone. Neither call follows the link on any platform.
        return std::fs::remove_file(path).or_else(|e| {
            if cfg!(windows) {
                std::fs::remove_dir(path)
            } else {
                Err(e)
            }
        });
    }
    if md.is_dir() {
        return std::fs::remove_dir_all(path);
    }
    std::fs::remove_file(path)
}

/// Makes `path` a real directory, replacing anything else that is sitting there.
///
/// The workspace home is bind-mounted into the sandbox **read-write** as `$HOME`
/// (`sandbox/linux_bwrap.rs`), so the agent owns every name under it: it can
/// `rm -rf ~/.claude` and leave a symlink to any host path in its place. A
/// dangling link is enough, because only the link *text* matters when the daemon
/// resolves it later on the host. `create_dir_all` follows such a link, and then
/// everything written "into the workspace home" lands outside it — the daemon
/// user's own `~/.claude/settings.json` is one `ln -s` away, and Claude Code
/// settings are hook commands, so that is code execution as the user.
///
/// `symlink_metadata` is the check that does not follow. Anything that is not a
/// real directory is unlinked and a real directory is created in its place; a
/// symlink found here is logged, because an agent that planted one was trying
/// something.
pub(crate) fn ensure_real_dir(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.is_dir() => return Ok(()),
        Ok(md) => {
            tracing::warn!(
                path = %path.display(),
                symlink = md.is_symlink(),
                "replacing something other than a real directory in the workspace home"
            );
            remove_any(path)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::create_dir_all(path)
}

/// Unlinks whatever is at `path` unless it is a real directory.
///
/// The directory is the one obstruction deliberately left in place: it cannot
/// redirect a write the way a symlink can, and removing it recursively would be
/// the daemon deleting data it did not put there. The `create_new` that follows
/// fails on it instead, which every caller reports.
pub(crate) fn clear_destination(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        return Ok(());
    }
    remove_any(path)
}

/// Writes `body` into `path` the way everything under `homes/<id>` is written:
/// the destination is unlinked first, never followed, and then created with
/// `create_new` so a symlink raced back in is refused rather than written
/// through. Daemon design §8.3.
pub(crate) fn write_guarded(path: &Path, body: &str) -> std::io::Result<()> {
    clear_destination(path)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(body.as_bytes())
}

/// Creates `path` for writing, refusing to reuse anything already there, and
/// on Unix with the private mode applied by `open` itself.
///
/// The mode matters at creation rather than afterwards: `fs::copy` would make
/// the file with the process umask, typically world-readable, and write the
/// OAuth tokens into it before any `chmod` could narrow it. The umask can only
/// clear bits, and 0600 has none to spare for group or other, so the file is
/// never wider than intended.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Copies one file, giving a secret a destination that is private from the
/// moment it exists.
///
/// Both kinds go through `create_new`, never `fs::copy`: `fs::copy` opens the
/// destination by path and follows a symlink found there, so a link raced back
/// in after [`remove_any`] cleared it would be written through. `create_new`
/// refuses instead.
fn copy_file(from: &Path, to: &Path, secret: bool) -> std::io::Result<()> {
    let mut src = std::fs::File::open(from)?;
    let mut dst = if secret {
        create_private(to)?
    } else {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)?
    };
    std::io::copy(&mut src, &mut dst)?;
    dst.flush()
}

/// Writes `body` into `path` with the private mode a secret gets, through the
/// same unlink-then-`create_new` the copies use.
fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    clear_destination(path)?;
    let mut f = create_private(path)?;
    f.write_all(body.as_bytes())
}

/// `source` with `worktree` marked as a trusted project, as the bytes to write.
///
/// Claude Code stores the answer to its trust dialog per project directory in
/// `~/.claude.json`, and reads a repository's `.claude/settings.json` — the
/// `permissions.allow` list a repo pins its agent's tools with — only for a
/// project it has been told to trust. A sandbox home is new every workspace and
/// has never seen this worktree, so without this entry the agent starts with the
/// repository's permissions dropped and no way to accept the dialog: it runs
/// non-interactively, and the daemon is what created the directory in the first
/// place.
///
/// Everything else in the file is the user's own and survives the merge. A
/// source that will not parse — or is not a JSON object — is replaced rather
/// than propagated: the CLI could not have read it either, and the alternative
/// is a workspace that stays untrusted for good.
pub(crate) fn claude_json_with_trust(source: Option<&str>, worktree: &Path) -> String {
    let parsed = source.and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
    let mut root = match parsed {
        Some(serde_json::Value::Object(map)) => map,
        _ => {
            if source.is_some_and(|s| !s.trim().is_empty()) {
                tracing::warn!(
                    "the daemon user's .claude.json is not a JSON object; seeding a fresh one"
                );
            }
            serde_json::Map::new()
        }
    };
    let projects = match root.get_mut("projects") {
        Some(serde_json::Value::Object(p)) => p,
        _ => {
            root.insert("projects".into(), serde_json::json!({}));
            root.get_mut("projects")
                .and_then(|v| v.as_object_mut())
                .expect("just inserted an object")
        }
    };
    // The worktree path *as the agent sees it*: the sandbox binds it at the same
    // path it has on the host (`workspace::lifecycle::spec_for`) and starts the
    // CLI with it as the working directory, so one string serves both sides.
    let key = worktree.to_string_lossy().into_owned();
    match projects.get_mut(&key) {
        Some(serde_json::Value::Object(entry)) => {
            entry.insert("hasTrustDialogAccepted".into(), true.into());
        }
        _ => {
            projects.insert(key, serde_json::json!({ "hasTrustDialogAccepted": true }));
        }
    }
    serde_json::Value::Object(root).to_string()
}

/// Mirrors the Claude Code login out of `source_home` into `home`, overwriting
/// what is already there so a fresh login refreshes an existing workspace,
/// removing what `source_home` no longer has so a logout does too, and marks
/// `worktree` as a project this home trusts.
///
/// Returns the relative names actually written, for logging; a name whose
/// source is gone is not among them, because nothing was written for it. Every
/// failure is non-fatal: a missing file simply means the user has not logged in
/// (or has logged out, or has no settings), and the agent will say so itself
/// when it starts. `.claude.json` is
/// the exception that is always written, because the trust entry has to be there
/// whether or not the daemon user has a file to merge it into.
pub fn seed_claude_files_from(
    source_home: &Path,
    home: &Path,
    worktree: &Path,
) -> Vec<&'static str> {
    let mut seeded = Vec::new();
    // The home itself before anything under it: `.claude.json` is written
    // directly into it, and it is as replaceable by the agent as `.claude` is.
    if let Err(e) = ensure_real_dir(home) {
        tracing::warn!(path = %home.display(), error = %e, "could not create the workspace home");
        return seeded;
    }
    for (rel, secret) in FILES {
        let from = source_home.join(rel);
        if rel == CLAUDE_JSON {
            // Written, not copied: whatever the daemon user has (or has not) is
            // merged with this workspace's trust entry. Straight into `home`,
            // which `ensure_real_dir` has just made a real directory.
            let to = home.join(rel);
            // A file that is there but unreadable is not the same as one that is
            // not there: the trust entry survives either way, but the second case
            // silently drops the user's account and onboarding state, so it is
            // said out loud the way every other seeding failure is.
            let source = match std::fs::read_to_string(&from) {
                Ok(s) => Some(s),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    tracing::warn!(from = %from.display(), error = %e, "could not read the daemon user's .claude.json; seeding a fresh one");
                    None
                }
            };
            let body = claude_json_with_trust(source.as_deref(), worktree);
            match write_private(&to, &body) {
                Ok(()) => seeded.push(rel),
                Err(e) => {
                    tracing::warn!(path = %to.display(), error = %e, "could not seed .claude.json")
                }
            }
            continue;
        }
        let to = home.join(rel);
        if !from.is_file() {
            // The daemon user has no such file, so neither may this workspace.
            //
            // Skipping instead would leave the *last* copy in place, and for
            // `.credentials.json` that copy is a working login: after
            // `claude auth logout` every existing workspace would go on
            // answering with the session the user had just put away, and the
            // logout would look like it had not worked. The seeding mirrors the
            // daemon user's home in both directions for that reason -- a file
            // that is gone there is gone here.
            if let Err(e) = clear_destination(&to) {
                tracing::warn!(path = %to.display(), error = %e, "could not remove a seeded file the daemon user no longer has");
            }
            continue;
        }
        if let Some(parent) = to.parent() {
            // `ensure_real_dir`, not `create_dir_all`: the agent can leave a
            // symlink at `<home>/.claude` pointing out of the workspace, and
            // `create_dir_all` would follow it and put the daemon user's own
            // credentials wherever it points.
            if let Err(e) = ensure_real_dir(parent) {
                tracing::warn!(path = %parent.display(), error = %e, "could not create claude dir");
                continue;
            }
        }
        // Cleared first, which is also what makes `create_new` succeed: it keeps
        // a stale mode from surviving a refresh, and unlinks a symlink planted at
        // the destination rather than writing through it.
        let _ = clear_destination(&to);
        match copy_file(&from, &to, secret) {
            Ok(()) => seeded.push(rel),
            Err(e) => {
                tracing::warn!(from = %from.display(), error = %e, "could not seed claude file")
            }
        }
    }
    seeded
}

/// [`seed_claude_files_from`] out of the daemon user's own home.
///
/// A platform that will not name a home still gets the trust entry: the merge
/// has nothing to merge into, which is the same case as a user who has never
/// logged in.
pub fn seed_claude_files(home: &Path, worktree: &Path) -> Vec<&'static str> {
    // Never a path under `home`: that one is the agent's to write, and a source
    // it could create is a source it could dictate.
    let source_home = daemon_home().unwrap_or_else(|| PathBuf::from("/nonexistent/no-daemon-home"));
    seed_claude_files_from(&source_home, home, worktree)
}

/// The login file, relative to a home; the same name [`FILES`] copies.
const CREDENTIALS: &str = ".claude/.credentials.json";

/// What one `.credentials.json` says, as far as the write-back needs to read
/// it. The file is `{"claudeAiOauth": {accessToken, refreshToken, expiresAt,
/// refreshTokenExpiresAt, scopes, subscriptionType, ...}}`; everything not
/// named here stays the CLI's business, because a write-back copies the bytes
/// and never re-serialises them.
#[derive(Debug, PartialEq)]
struct Login {
    access_token: String,
    refresh_token: String,
    scopes: Option<serde_json::Value>,
    subscription_type: Option<serde_json::Value>,
    expires_at: Option<i64>,
    refresh_token_expires_at: Option<i64>,
}

impl Login {
    /// `None` when the bytes are not a credentials file at all: not JSON, or
    /// JSON with no `claudeAiOauth` object in it.
    fn parse(bytes: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
        let oauth = v.get("claudeAiOauth")?.as_object()?;
        let string = |k: &str| {
            oauth
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        let stamp = |k: &str| oauth.get(k).and_then(|v| v.as_i64());
        Some(Self {
            access_token: string("accessToken"),
            refresh_token: string("refreshToken"),
            scopes: oauth.get("scopes").cloned(),
            subscription_type: oauth.get("subscriptionType").cloned(),
            expires_at: stamp("expiresAt"),
            refresh_token_expires_at: stamp("refreshTokenExpiresAt"),
        })
    }

    /// Whether this file would log anyone in. The file the CLI leaves behind
    /// after a refresh that failed parses fine and has both tokens empty.
    fn is_live(&self) -> bool {
        !self.access_token.is_empty() && !self.refresh_token.is_empty()
    }
}

/// Why a workspace copy was not written back; each is one `warn!`, and a copy
/// that is merely not newer or not different is none, because that is the
/// ordinary state of every workspace.
#[derive(Debug, PartialEq)]
enum Rejected {
    Symlink,
    Unparsable,
    Emptied,
    OtherAccount,
}

/// The host's copy, as the write-back compares against it: its mtime, and the
/// login in it -- `None` when the file does not parse, which is compared
/// against as "nothing matches".
struct Host {
    mtime: SystemTime,
    login: Option<Login>,
}

/// Copies the login in `home` back over the daemon user's when it is a newer,
/// working copy of the same login. Answers whether it did.
///
/// Newer is by mtime: the CLI rewrites the file when it refreshes, and the
/// host's own CLI does the same, so a copy older than the host's is a workspace
/// that has not refreshed since the user last did, and its refresh token is the
/// dead one.
///
/// Working is the one condition that keeps this from doing harm. A refresh
/// that fails -- with the dead token this feature exists to stop seeding --
/// makes the CLI write the file back with **empty** `accessToken` and
/// `refreshToken` and `expiresAt: 0`. That file is newer than the host's, and
/// copying it back would wipe a live host login on the strength of a workspace
/// that had already lost its own. So the copy must parse and carry both tokens,
/// or it is refused and said so in the log.
///
/// Same login is the rest, and it answers a different threat. The workspace
/// home is the agent's to write, so the copy can be anything: an agent that
/// planted the tokens of an account it controls, with a fresh mtime, would
/// have the daemon replace the user's login with them, and every later `claude`
/// the user ran on the host would run as that account. Nothing in the file
/// names the account, so it cannot be checked outright; what can be is that a
/// refresh changes only the tokens and their expiries. A copy is accepted only
/// when its `scopes` and `subscriptionType` equal the host's exactly, neither
/// expiry has moved backwards, and the tokens actually differ (a copy that
/// equals the host's has nothing to bring). A planted file from another
/// account of the same tier and scopes still passes, and that is stated here
/// rather than hidden: the agent already holds the user's live tokens, so the
/// bar this raises is against substitution, not disclosure, and every
/// write-back is logged at `info` with the workspace it came from so a
/// substituted login is a line in the log, not a mystery.
///
/// A host with no login at all gets none: the user has logged out, or never
/// logged in, and a workspace restoring a login the user put away is the very
/// thing the mirror in [`seed_claude_files_from`] exists to stop. A host whose
/// file is *there* but not a working login -- the emptied file, this time on
/// the host, from a refresh that failed because a workspace had rotated the
/// token first -- is the case worth having: that is the login coming back from
/// the one place it still works.
///
/// Both files are handled as their owners deserve. The workspace copy is read
/// without following a symlink at its path, or at `.claude` above it, since
/// either is the agent's to plant. The host's is replaced by a rename of a
/// private temporary, so there is never a moment with no login file and never
/// a write through whatever the destination name resolves to; a host
/// destination that is itself a symlink is the user's own arrangement and is
/// refused rather than replaced.
pub fn write_back_login_from(home: &Path, host_home: &Path) -> bool {
    let ws = home
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let _serialised = WRITE_BACK.lock();
    // The host first: with no login there, nothing may be restored.
    let Some(host) = host_login(host_home) else {
        return false;
    };
    let from = home.join(CREDENTIALS);
    let candidate = match refreshed_login(&from, &host) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return false,
        Err(why) => {
            tracing::warn!(ws = %ws, path = %from.display(), why = ?why, "a workspace login was not written back");
            return false;
        }
    };
    let to = host_home.join(CREDENTIALS);
    match replace_private(&to, &candidate) {
        Ok(()) => {
            tracing::info!(ws = %ws, "a login the workspace refreshed was written back to the daemon user");
            true
        }
        Err(e) => {
            tracing::warn!(ws = %ws, path = %to.display(), error = %e, "could not write a refreshed login back");
            false
        }
    }
}

/// [`write_back_login_from`] into the daemon user's own home.
pub fn write_back_login(home: &Path) -> bool {
    daemon_home().is_some_and(|host| write_back_login_from(home, &host))
}

/// The newest refreshed login among every home under `homes`, written back.
///
/// Before a workspace is seeded, not only when an agent exits: the workspace
/// that refreshed may still be running, and the one about to start would
/// otherwise be handed the host's stale copy. Newest first, and done at the
/// first that is written, because from then on the host is the newest.
pub fn write_back_any_refreshed_login_from(homes: &Path, host_home: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(homes) else {
        return false;
    };
    let mut candidates: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter_map(|home| {
            let md = std::fs::symlink_metadata(home.join(CREDENTIALS)).ok()?;
            Some((md.modified().ok()?, home))
        })
        .collect();
    candidates.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    candidates
        .iter()
        .any(|(_, home)| write_back_login_from(home, host_home))
}

/// [`write_back_any_refreshed_login_from`] into the daemon user's own home.
pub fn write_back_any_refreshed_login(homes: &Path) -> bool {
    daemon_home().is_some_and(|host| write_back_any_refreshed_login_from(homes, &host))
}

/// One write-back at a time. Two agents can exit together, and a start's sweep
/// can meet an exit; each compares the host's file and then replaces it, and
/// interleaved they would each have compared against a file the other was
/// about to replace.
static WRITE_BACK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// The host's login file as the write-back compares against it, or `None` when
/// there is none to compare against -- absent, a symlink, or not a file.
fn host_login(host_home: &Path) -> Option<Host> {
    let path = host_home.join(CREDENTIALS);
    let md = match std::fs::symlink_metadata(&path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not look at the daemon user's login");
            return None;
        }
    };
    if md.is_symlink() || !md.is_file() {
        tracing::warn!(path = %path.display(), "the daemon user's login is not a plain file; no login is written back to it");
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    Some(Host {
        mtime: md.modified().ok()?,
        login: Login::parse(&bytes),
    })
}

/// The bytes at `from` when they are a newer, working copy of `host`'s login;
/// `Ok(None)` when they are nothing to act on, and the reason when they were
/// refused.
fn refreshed_login(from: &Path, host: &Host) -> Result<Option<Vec<u8>>, Rejected> {
    // `.claude` above the file as well as the file: a directory symlink there
    // would make the path below it resolve anywhere.
    let dir_is_real = from
        .parent()
        .and_then(|p| std::fs::symlink_metadata(p).ok())
        .is_some_and(|md| md.is_dir() && !md.is_symlink());
    if !dir_is_real {
        return Ok(None);
    }
    let md = match std::fs::symlink_metadata(from) {
        Ok(md) => md,
        Err(_) => return Ok(None),
    };
    if md.is_symlink() {
        return Err(Rejected::Symlink);
    }
    if !md.is_file() || md.modified().ok().is_none_or(|m| m <= host.mtime) {
        return Ok(None);
    }
    let bytes = read_without_following(from).map_err(|_| Rejected::Symlink)?;
    let login = Login::parse(&bytes).ok_or(Rejected::Unparsable)?;
    if !login.is_live() {
        return Err(Rejected::Emptied);
    }
    let Some(theirs) = &host.login else {
        // A host file that is not a credentials file at all is not one this
        // can vouch for a copy of.
        return Err(Rejected::OtherAccount);
    };
    let same_login = login.scopes == theirs.scopes
        && login.subscription_type == theirs.subscription_type
        && login.expires_at >= theirs.expires_at
        && login.refresh_token_expires_at >= theirs.refresh_token_expires_at;
    if !same_login {
        return Err(Rejected::OtherAccount);
    }
    if login.access_token == theirs.access_token && login.refresh_token == theirs.refresh_token {
        return Ok(None);
    }
    Ok(Some(bytes))
}

/// Reads `path` refusing, on Unix, to follow a symlink at its final component
/// even if one was raced in after the caller looked. The sandbox, and so the
/// agent that could race, is Unix-only; elsewhere the caller's look suffices.
fn read_without_following(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = opts.open(path)?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut f, &mut bytes)?;
    Ok(bytes)
}

/// Replaces the file at `path` with `body`, private from the moment it exists
/// and never absent: the body goes into a sibling temporary made the way
/// [`create_private`] makes every secret, is synced, and is renamed over the
/// destination. A rename replaces a name and follows nothing, and a temporary
/// that was already there -- which only another daemon could have left -- is
/// refused by `create_new` rather than reused.
pub(crate) fn replace_private(path: &Path, body: &[u8]) -> std::io::Result<()> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let tmp_name = format!(
        ".{}.{}-{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    );
    let tmp = path.with_file_name(tmp_name);
    let written = (|| {
        let mut f = create_private(&tmp)?;
        f.write_all(body)?;
        // Before the rename: a crash between an unsynced rename and the data
        // reaching the disk can leave the name pointing at an empty file, and
        // an empty login file is a logged-out user.
        f.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn write(path: PathBuf, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// A directory symlink at `link`, or `false` where the platform refuses to
    /// make one (Windows without the symlink privilege). Unix, which is where
    /// the sandbox and therefore the attack live, always makes it.
    fn symlink_dir(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
        }
    }

    #[test]
    fn only_the_files_that_exist_are_seeded_and_secrets_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        write(src.join(".claude/.credentials.json"), "{\"t\":1}");
        write(src.join(".claude.json"), "{}");
        // No settings.json: it must simply be skipped.

        let seeded = seed_claude_files_from(&src, &dst, &worktree);
        assert_eq!(seeded, vec![".claude/.credentials.json", ".claude.json"]);
        assert_eq!(
            std::fs::read_to_string(dst.join(".claude/.credentials.json")).unwrap(),
            "{\"t\":1}"
        );
        assert!(!dst.join(".claude/settings.json").exists());
        #[cfg(unix)]
        for rel in [".claude/.credentials.json", ".claude.json"] {
            assert_eq!(mode_of(&dst.join(rel)), 0o600, "{rel} must be private");
        }
    }

    /// The point of seeding again at every `agent.start`: a user who logs in
    /// after the workspace was created must get the new tokens, and a
    /// destination someone left world-readable must not stay that way.
    #[test]
    fn seeding_again_refreshes_the_content_and_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        write(src.join(".claude/.credentials.json"), "old");
        write(src.join(".claude/settings.json"), "{\"a\":1}");
        seed_claude_files_from(&src, &dst, &worktree);

        // A stale, wide-open destination, as an earlier version of this code
        // (or a user) could have left behind.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                dst.join(".claude/.credentials.json"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        write(src.join(".claude/.credentials.json"), "new");

        let seeded = seed_claude_files_from(&src, &dst, &worktree);
        assert_eq!(
            seeded,
            vec![
                ".claude/settings.json",
                ".claude/.credentials.json",
                ".claude.json"
            ]
        );
        assert_eq!(
            std::fs::read_to_string(dst.join(".claude/.credentials.json")).unwrap(),
            "new",
            "a later login must overwrite the older one"
        );
        #[cfg(unix)]
        assert_eq!(
            mode_of(&dst.join(".claude/.credentials.json")),
            0o600,
            "a refresh must not leave a stale mode in place"
        );
    }

    /// One file that cannot be written is logged and skipped, and the rest of
    /// the seeding still happens. Seeding is best effort: a workspace that gets
    /// most of a login is more useful than one that gets none.
    ///
    /// The obstruction is a real *directory* at a destination file. That is the
    /// one thing the seeding does not clear: a symlink or a file in the way is
    /// unlinked (see the two tests below), because leaving either there is how a
    /// write escapes the home, but a directory cannot redirect anything and
    /// removing it recursively would be this code deleting data it did not put
    /// there.
    #[test]
    fn a_destination_that_cannot_be_written_does_not_stop_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        write(src.join(".claude/.credentials.json"), "secret");
        write(src.join(".claude.json"), "{}");
        std::fs::create_dir_all(dst.join(".claude/.credentials.json")).unwrap();

        let seeded = seed_claude_files_from(&src, &dst, &worktree);
        assert_eq!(seeded, vec![".claude.json"]);
        assert!(std::fs::read_to_string(dst.join(".claude.json"))
            .unwrap()
            .contains("hasTrustDialogAccepted"));
        assert!(
            dst.join(".claude/.credentials.json").is_dir(),
            "the obstruction is reported, not removed"
        );
    }

    /// C1: the agent owns `$HOME` inside the sandbox, so `.claude` can be a
    /// symlink out of the workspace by the time the daemon seeds it again. The
    /// link must be replaced, not followed.
    #[test]
    fn a_symlinked_claude_dir_is_replaced_and_its_target_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        let outside = dir.path().join("the-users-real-home").join(".claude");
        write(
            src.join(".claude/.credentials.json"),
            "the daemon user's tokens",
        );
        write(outside.join("settings.json"), "the user's own settings");

        std::fs::create_dir_all(&dst).unwrap();
        if !symlink_dir(&outside, &dst.join(".claude")) {
            eprintln!("SKIP: this host will not create directory symlinks");
            return;
        }

        let seeded = seed_claude_files_from(&src, &dst, &worktree);
        assert_eq!(seeded, vec![".claude/.credentials.json", ".claude.json"]);
        assert!(
            std::fs::symlink_metadata(dst.join(".claude"))
                .unwrap()
                .is_dir(),
            "the link was replaced by a real directory"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join(".claude/.credentials.json")).unwrap(),
            "the daemon user's tokens"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("settings.json")).unwrap(),
            "the user's own settings",
            "nothing outside the workspace home may be touched"
        );
        assert!(
            !outside.join(".credentials.json").exists(),
            "nothing may be written through the link"
        );
    }

    /// The same guard with no symlink privilege needed, so it runs on Windows
    /// too: anything that is not a real directory is replaced.
    #[test]
    fn a_file_where_the_claude_dir_goes_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        write(src.join(".claude/.credentials.json"), "tokens");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join(".claude"), "in the way").unwrap();

        let seeded = seed_claude_files_from(&src, &dst, &worktree);
        assert_eq!(seeded, vec![".claude/.credentials.json", ".claude.json"]);
        assert!(dst.join(".claude").is_dir());
        assert_eq!(
            std::fs::read_to_string(dst.join(".claude/.credentials.json")).unwrap(),
            "tokens"
        );
    }

    /// `write_guarded` is what `seed_home` writes `.gitconfig` through, so the
    /// spec's claim that *every* write into `homes/<id>` de-symlinks its path is
    /// true of that one too.
    #[test]
    fn a_guarded_write_unlinks_a_symlink_rather_than_writing_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("the-users-real.gitconfig");
        std::fs::write(&outside, "the user's own").unwrap();
        let home = dir.path().join("ws-home");
        std::fs::create_dir_all(&home).unwrap();
        let to = home.join(".gitconfig");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &to).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&outside, &to).is_err() {
            eprintln!("SKIP: this host will not create file symlinks");
            return;
        }

        write_guarded(&to, "ours").unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "ours");
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "the user's own",
            "the link's target must be untouched"
        );
    }

    /// And a real directory in the way is left there rather than deleted.
    #[test]
    fn a_guarded_write_leaves_a_real_directory_alone() {
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join(".gitconfig");
        std::fs::create_dir_all(to.join("something")).unwrap();
        assert!(write_guarded(&to, "ours").is_err());
        assert!(to.join("something").is_dir(), "nothing was deleted");
    }

    /// Whatever this machine's daemon home happens to hold, the public entry
    /// point creates exactly the files it reports and no others.
    #[test]
    fn seeding_creates_exactly_what_it_reports() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        let seeded = seed_claude_files(&dst, &worktree);
        for rel in &seeded {
            assert!(
                dst.join(rel).is_file(),
                "{rel} was reported but not written"
            );
        }
        for (rel, _) in FILES {
            if !seeded.contains(&rel) {
                assert!(
                    !dst.join(rel).exists(),
                    "{rel} appeared without being reported"
                );
            }
        }
    }

    /// The bug this fixes, in the daemon log of the first real run:
    /// `claude: Ignoring 9 permissions.allow entries from .claude/settings.json:
    /// this workspace has not been trusted`. Claude Code keeps that consent per
    /// project directory in `~/.claude.json`, and the sandbox home is a fresh
    /// one every workspace — so unless the daemon writes the entry, a repository
    /// that pins the tools its agent may use has those settings ignored, and
    /// nobody inside the sandbox can answer the dialog that would fix it.
    ///
    /// Everything else in the file is the user's own and is carried across
    /// untouched: their account, their other projects, their onboarding state.
    #[test]
    fn the_seeded_claude_json_trusts_the_worktree_and_keeps_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("worktrees").join("ws_1234");
        write(
            src.join(".claude.json"),
            r#"{
              "userID": "u-1",
              "hasCompletedOnboarding": true,
              "projects": {
                "/home/someone/other": {"hasTrustDialogAccepted": true, "history": ["a"]}
              }
            }"#,
        );

        seed_claude_files_from(&src, &dst, &worktree);

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dst.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(v["userID"], "u-1");
        assert_eq!(v["hasCompletedOnboarding"], true);
        assert_eq!(v["projects"]["/home/someone/other"]["history"][0], "a");
        assert_eq!(
            v["projects"][worktree.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            true
        );
    }

    /// A daemon user who has never run Claude Code has no `.claude.json` at all,
    /// and the workspace still needs one: the trust entry is the point of the
    /// file here, not a decoration on a copy.
    #[test]
    fn a_home_with_no_claude_json_still_gets_a_trusted_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        std::fs::create_dir_all(&src).unwrap();
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");

        let seeded = seed_claude_files_from(&src, &dst, &worktree);

        assert_eq!(seeded, vec![".claude.json"]);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dst.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["projects"][worktree.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            true
        );
        #[cfg(unix)]
        assert_eq!(
            mode_of(&dst.join(".claude.json")),
            0o600,
            "it is written the way the copy of it would be"
        );
    }

    /// A `.claude.json` that will not parse cannot be merged into, and refusing
    /// to seed over it would leave the workspace untrusted for good. The trust
    /// entry wins; what is lost is a file the CLI could not have read either.
    #[test]
    fn an_unparseable_claude_json_still_yields_a_trusted_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        write(src.join(".claude.json"), "half a file {");

        seed_claude_files_from(&src, &dst, &worktree);

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dst.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["projects"][worktree.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            true
        );
    }

    /// The merge is where a mistake would be silent, so its edges are checked
    /// directly: a `projects` key of the wrong shape must not make the whole
    /// file unwritable, and an entry that is already there keeps its own fields.
    #[test]
    fn the_trust_merge_replaces_only_what_it_has_to() {
        let wt = Path::new("/w/t");
        let key = "/w/t";

        let v: serde_json::Value = serde_json::from_str(&claude_json_with_trust(
            Some(r#"{"projects": {"/w/t": {"history": ["x"], "hasTrustDialogAccepted": false}}}"#),
            wt,
        ))
        .unwrap();
        assert_eq!(v["projects"][key]["hasTrustDialogAccepted"], true);
        assert_eq!(v["projects"][key]["history"][0], "x");

        // `projects` as something other than an object, and a top level that is
        // not an object at all: both are replaced rather than propagated.
        for source in [r#"{"projects": 7}"#, "[1, 2]", "null"] {
            let v: serde_json::Value =
                serde_json::from_str(&claude_json_with_trust(Some(source), wt)).unwrap();
            assert_eq!(
                v["projects"][key]["hasTrustDialogAccepted"], true,
                "source {source}"
            );
        }
    }

    /// A `.claude.json` that is there but cannot be read is not the same as one
    /// that is absent: the trust entry still has to land, and the daemon says so
    /// in the log rather than silently dropping the user's account state. A
    /// directory at the source path is the portable way to make the read fail.
    #[test]
    fn a_source_claude_json_that_cannot_be_read_still_yields_a_trusted_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        std::fs::create_dir_all(src.join(".claude.json")).unwrap();

        let seeded = seed_claude_files_from(&src, &dst, &worktree);

        assert_eq!(seeded, vec![".claude.json"]);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dst.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["projects"][worktree.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            true
        );
    }

    /// What makes the Setup page's Log out button mean anything for a workspace
    /// that already exists.
    ///
    /// `claude auth logout` removes the daemon user's `.credentials.json`. Every
    /// workspace created before that has a copy of it, and the copy is a working
    /// login: a seeding that only ever added files would leave each one able to
    /// answer as the account the user had just signed out of. The removal has to
    /// travel the same way the login does.
    #[test]
    fn a_login_the_daemon_user_no_longer_has_is_taken_out_of_the_workspace_too() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        write(src.join(".claude/.credentials.json"), "{\"t\":1}");
        write(src.join(".claude/settings.json"), "{\"a\":1}");
        seed_claude_files_from(&src, &dst, &worktree);
        assert!(dst.join(".claude/.credentials.json").is_file());

        // The logout: the tokens go, the settings stay.
        std::fs::remove_file(src.join(".claude/.credentials.json")).unwrap();

        let seeded = seed_claude_files_from(&src, &dst, &worktree);
        assert!(
            !seeded.contains(&".claude/.credentials.json"),
            "nothing was written for it: {seeded:?}"
        );
        assert!(
            !dst.join(".claude/.credentials.json").exists(),
            "the workspace must not keep a login the daemon user has put away"
        );
        assert!(
            dst.join(".claude/settings.json").is_file(),
            "a file the daemon user still has is untouched"
        );
    }

    /// The mirror unlinks rather than following, the same as every other write
    /// into a workspace home. An agent can leave a symlink at the destination,
    /// and a removal that followed one would delete the daemon user's own
    /// credentials on the host -- the very file the seeding reads.
    #[test]
    fn a_symlink_at_a_vanished_files_destination_is_unlinked_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src-home");
        let dst = dir.path().join("ws-home");
        let worktree = dir.path().join("wt");
        // A file the agent would like removed, standing in for anything on the
        // host that is not this workspace's business.
        let bait = dir.path().join("bait");
        std::fs::write(&bait, "still here").unwrap();
        // Only settings in the source, so `.credentials.json` is the vanished
        // one on the very first seeding.
        write(src.join(".claude/settings.json"), "{}");
        std::fs::create_dir_all(dst.join(".claude")).unwrap();
        let planted = dst.join(".claude/.credentials.json");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&bait, &planted).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&bait, &planted).is_ok();
        if !made {
            return;
        }

        seed_claude_files_from(&src, &dst, &worktree);

        assert!(
            std::fs::symlink_metadata(&planted).is_err(),
            "the planted link must be gone"
        );
        assert_eq!(
            std::fs::read_to_string(&bait).unwrap(),
            "still here",
            "its target must be untouched"
        );
    }

    /// A credentials file the shape the CLI writes, with the fields the
    /// write-back compares fixed so two of these differ only in what a refresh
    /// changes.
    fn creds(access: &str, refresh: &str, expires_at: i64) -> String {
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access,
                "refreshToken": refresh,
                "expiresAt": expires_at,
                "refreshTokenExpiresAt": 1_800_000_000_000i64,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "max",
                "rateLimitTier": "default_claude_max_5x",
            }
        })
        .to_string()
    }

    /// The tokens as the host held them, and as a workspace's refresh rotated
    /// them: same account, later expiry, both tokens new.
    const HOST: (&str, &str, i64) = ("sk-ant-oat01-host", "sk-ant-ort01-host", 1_000);
    const REFRESHED: (&str, &str, i64) = ("sk-ant-oat01-new", "sk-ant-ort01-new", 2_000);

    /// Writes `body` at `path` with an mtime `secs` after a fixed origin, so a
    /// test says which file is newer instead of relying on the clock and the
    /// filesystem's idea of resolution.
    fn write_at(path: PathBuf, body: &str, secs: u64) {
        write(path.clone(), body);
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    /// The bug, seen end to end: the workspace refreshed and the host still
    /// holds the rotated-away refresh token. The refreshed copy comes back
    /// byte for byte, private, and a second look finds nothing left to bring.
    #[test]
    fn a_login_a_workspace_refreshed_is_written_back_to_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host-home");
        let ws = dir.path().join("homes").join("ws_1");
        write_at(host.join(CREDENTIALS), &creds(HOST.0, HOST.1, HOST.2), 0);
        let refreshed = creds(REFRESHED.0, REFRESHED.1, REFRESHED.2);
        write_at(ws.join(CREDENTIALS), &refreshed, 60);

        assert!(write_back_login_from(&ws, &host));
        assert_eq!(
            std::fs::read_to_string(host.join(CREDENTIALS)).unwrap(),
            refreshed
        );
        #[cfg(unix)]
        assert_eq!(mode_of(&host.join(CREDENTIALS)), 0o600);
        assert!(
            !write_back_login_from(&ws, &host),
            "the host has these tokens now; there is nothing to write"
        );
    }

    /// The one way this feature could do harm. After a refresh fails the CLI
    /// writes the file back with both tokens empty, and that file is newer
    /// than the host's. It must never land on the host: the host's login may
    /// well be the working one.
    ///
    /// Twice: as the CLI really writes it, with `expiresAt: 0`, which the
    /// expiry rule would refuse on its own; and with the expiries left intact,
    /// so the empty tokens are the only thing wrong with it and the check on
    /// them is shown to be load-bearing by itself.
    #[test]
    fn an_emptied_login_is_never_written_back() {
        for emptied in [creds("", "", 0), creds("", "", REFRESHED.2)] {
            let dir = tempfile::tempdir().unwrap();
            let host = dir.path().join("host-home");
            let ws = dir.path().join("homes").join("ws_1");
            let live = creds(HOST.0, HOST.1, HOST.2);
            write_at(host.join(CREDENTIALS), &live, 0);
            write_at(ws.join(CREDENTIALS), &emptied, 60);

            assert!(!write_back_login_from(&ws, &host), "{emptied}");
            assert_eq!(
                std::fs::read_to_string(host.join(CREDENTIALS)).unwrap(),
                live,
                "a dead workspace login must not wipe a live host one"
            );
        }
    }

    /// The user logged in again on the host after this workspace refreshed:
    /// the host's copy is the newer, and the workspace's refresh token is the
    /// one that is dead.
    ///
    /// The relogin's expiries are deliberately no later than the workspace
    /// copy's. Ordinarily they would be, and the expiry rule would refuse the
    /// copy by itself; here only its age says no, so this is the mtime rule
    /// pinned on its own.
    #[test]
    fn an_older_workspace_copy_is_not_written_back() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host-home");
        let ws = dir.path().join("homes").join("ws_1");
        let relogin = creds("sk-ant-oat01-relogin", "sk-ant-ort01-relogin", HOST.2);
        write_at(host.join(CREDENTIALS), &relogin, 60);
        write_at(
            ws.join(CREDENTIALS),
            &creds(REFRESHED.0, REFRESHED.1, REFRESHED.2),
            0,
        );

        assert!(!write_back_login_from(&ws, &host));
        assert_eq!(
            std::fs::read_to_string(host.join(CREDENTIALS)).unwrap(),
            relogin
        );
    }

    /// The substitution the same-login rule is for: a planted file from some
    /// other account, fresher than the host's and a working login in its own
    /// right, is refused because what a refresh does not change has changed.
    #[test]
    fn a_login_from_another_account_is_not_written_back() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host-home");
        let ws = dir.path().join("homes").join("ws_1");
        let live = creds(HOST.0, HOST.1, HOST.2);
        write_at(host.join(CREDENTIALS), &live, 0);
        let planted = creds(REFRESHED.0, REFRESHED.1, REFRESHED.2).replace("\"max\"", "\"pro\"");
        write_at(ws.join(CREDENTIALS), &planted, 60);

        assert!(!write_back_login_from(&ws, &host));
        assert_eq!(
            std::fs::read_to_string(host.join(CREDENTIALS)).unwrap(),
            live
        );
    }

    /// A host with no login has logged out, and a workspace may not undo
    /// that -- the same rule the mirror in the seeding keeps.
    #[test]
    fn a_host_with_no_login_gets_none_back() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host-home");
        std::fs::create_dir_all(host.join(".claude")).unwrap();
        let ws = dir.path().join("homes").join("ws_1");
        write_at(
            ws.join(CREDENTIALS),
            &creds(REFRESHED.0, REFRESHED.1, REFRESHED.2),
            60,
        );

        assert!(!write_back_login_from(&ws, &host));
        assert!(!host.join(CREDENTIALS).exists());
    }

    /// The workspace copy's path is the agent's to plant a symlink at. The
    /// bait it points at is a perfectly good refreshed login, newer than the
    /// host's; through the link it must be neither read nor written back.
    #[test]
    fn a_symlink_planted_at_the_workspace_copy_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host-home");
        let ws = dir.path().join("homes").join("ws_1");
        let live = creds(HOST.0, HOST.1, HOST.2);
        write_at(host.join(CREDENTIALS), &live, 0);
        let bait = dir.path().join("bait");
        let baited = creds(REFRESHED.0, REFRESHED.1, REFRESHED.2);
        write_at(bait.clone(), &baited, 60);
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        let planted = ws.join(CREDENTIALS);
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&bait, &planted).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&bait, &planted).is_ok();
        if !made {
            eprintln!("SKIP: this host will not create file symlinks");
            return;
        }

        assert!(!write_back_login_from(&ws, &host));
        assert_eq!(
            std::fs::read_to_string(host.join(CREDENTIALS)).unwrap(),
            live,
            "nothing may reach the host through the link"
        );
        assert_eq!(std::fs::read_to_string(&bait).unwrap(), baited);
        assert!(
            std::fs::symlink_metadata(&planted).unwrap().is_symlink(),
            "the link is refused, not replaced: this is a read, not a seeding"
        );
    }

    /// The host side of the same discipline: the destination is the user's
    /// own, and a symlink there is their arrangement, so it is neither written
    /// through nor swapped out for a real file.
    #[test]
    fn a_symlinked_host_login_is_refused_rather_than_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host-home");
        let ws = dir.path().join("homes").join("ws_1");
        let elsewhere = dir.path().join("dotfiles").join("credentials.json");
        let live = creds(HOST.0, HOST.1, HOST.2);
        write_at(elsewhere.clone(), &live, 0);
        std::fs::create_dir_all(host.join(".claude")).unwrap();
        let link = host.join(CREDENTIALS);
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&elsewhere, &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&elsewhere, &link).is_ok();
        if !made {
            eprintln!("SKIP: this host will not create file symlinks");
            return;
        }
        write_at(
            ws.join(CREDENTIALS),
            &creds(REFRESHED.0, REFRESHED.1, REFRESHED.2),
            60,
        );

        assert!(!write_back_login_from(&ws, &host));
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), live);
    }

    /// Why the sweep runs before every seeding: workspace A refreshed and is
    /// still running, and workspace B is about to start. B must get A's
    /// tokens, not the host's dead ones -- and the sweep takes the newest, so
    /// a third workspace's older copy is not what wins.
    #[test]
    fn seeding_after_a_write_back_hands_the_new_tokens_to_the_next_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("host-home");
        let homes = dir.path().join("homes");
        write_at(host.join(CREDENTIALS), &creds(HOST.0, HOST.1, HOST.2), 0);
        let refreshed = creds(REFRESHED.0, REFRESHED.1, REFRESHED.2);
        write_at(homes.join("ws_a").join(CREDENTIALS), &refreshed, 120);
        write_at(
            homes.join("ws_c").join(CREDENTIALS),
            &creds("sk-ant-oat01-mid", "sk-ant-ort01-mid", 1_500),
            60,
        );

        assert!(write_back_any_refreshed_login_from(&homes, &host));
        let b = homes.join("ws_b");
        let seeded = seed_claude_files_from(&host, &b, &dir.path().join("wt"));
        assert!(seeded.contains(&CREDENTIALS));
        assert_eq!(
            std::fs::read_to_string(b.join(CREDENTIALS)).unwrap(),
            refreshed,
            "the next workspace starts with the tokens that work"
        );
    }
}
