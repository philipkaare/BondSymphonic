//! The one rule for what a workspace may be called.
//!
//! A workspace name is not a label: it is spliced into the git branch
//! `bs/<name>/work`, into the loose-ref directory `refs/heads/bs/<name>/` and
//! its reflog, and it travels through a shell-free but still path-sensitive
//! chain of directory names on the daemon's side. So the question "is this name
//! allowed" has exactly one right answer, and it has to be the same answer on
//! both sides of the wire.
//!
//! It lives in `proto` because that is the only crate both the IDE and the
//! daemon depend on. Each of them used to carry its own version — the daemon
//! rejected an empty name, a `/`, a `..` and whitespace, the IDE had a rule of
//! its own — and neither matched what git accepts as a ref. `feat:x` passed
//! both and then failed inside `git worktree add`, *after* the client had been
//! told the workspace was being created.
//!
//! The rule is `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`, which is deliberately
//! narrower than git's own `check-ref-format`. Everything git merely
//! discourages — a leading dot, a trailing `.lock`, `@{`, a `.` anywhere — is
//! refused here instead of being reasoned about, because the name is also a
//! directory name and the cost of being narrow is that somebody types a hyphen
//! where they wanted a space.

/// The longest name allowed, in characters.
///
/// Counted in characters rather than bytes so that a name is never refused as
/// "too long" for a reason invisible in the text box. A multi-byte name is
/// refused anyway, for its characters — which is a reason the user can act on.
pub const MAX_LEN: usize = 64;

/// `Ok(())` when `name` may become a workspace, otherwise the sentence to show
/// the user.
///
/// The error is the whole user-facing reason, ready to put next to the field:
/// callers add no wording of their own. There are three, and which one comes
/// back is decided in the order a person would notice them — nothing typed
/// yet, too much typed, the wrong characters typed:
///
/// * `"name is empty"`
/// * `"name is too long (64 max)"`
/// * `"use letters, digits, - or _"`
///
/// The last one also covers a name that *starts* with `-` or `_`. A separate
/// message for that case would be more precise, but three messages are what
/// both sides of the wire agree on, and the one that is shown still names the
/// characters a valid name is made of.
pub fn validate(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name is empty".to_string());
    }
    if name.chars().count() > MAX_LEN {
        return Err(format!("name is too long ({MAX_LEN} max)"));
    }
    let bad = "use letters, digits, - or _";
    let mut chars = name.chars();
    // The first character carries its own rule: a name that begins with `-`
    // reads as an option to every command line it is ever pasted into, and one
    // that begins with `_` is a hidden-ish directory on more tools than it is
    // worth listing.
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return Err(bad.to_string()),
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(bad.to_string());
    }
    Ok(())
}
