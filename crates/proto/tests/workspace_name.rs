//! The one workspace-name rule, checked from the outside the way both sides
//! call it.
//!
//! A workspace name is not free text: it becomes the git branch
//! `bs/<name>/work`, a directory name under the daemon's data root and part of
//! `refs/heads/bs/<name>/`. The daemon and the IDE used to each carry their own
//! idea of what was allowed, and neither matched what git accepts as a ref — so
//! a name the dialog took happily could only fail somewhere inside
//! `git worktree add`, after the workspace had already been announced to the
//! client.

use bondsymphonic_proto::workspace_name::validate;

/// Every name git or the filesystem would refuse, and the reason a user sees.
#[test]
fn names_that_cannot_become_a_branch_are_refused() {
    for name in [
        // `:` is a ref-format character git rejects outright; the old daemon
        // check let it through and `git worktree add -b bs/feat:x/work` failed.
        "feat:x",
        "a b",
        "a/b",
        "..",
        "a..b",
        "a~b",
        "a^b",
        "a?b",
        "a*b",
        "a[b",
        "a\\b",
        "a\tb",
        ".hidden",
        "-leading-dash",
        "_leading-underscore",
        "café",
        "a.lock",
        "a.b",
    ] {
        assert!(
            validate(name).is_err(),
            "{name:?} must be refused: it cannot be part of a git ref or a directory name"
        );
    }
}

/// The ordinary names, including the two shapes the review named: a hyphen and
/// an underscore, and capitals.
#[test]
fn ordinary_names_are_accepted() {
    for name in [
        "feat-1",
        "Feature_A",
        "a",
        "9",
        "alpha",
        "fix-the-parser",
        "A_B-c9",
        &"x".repeat(64),
    ] {
        assert_eq!(validate(name), Ok(()), "{name:?} must be accepted");
    }
}

/// The three messages are what the user reads in the New Agent dialog, so each
/// one has to say which of the three things is wrong.
#[test]
fn the_message_names_the_problem() {
    assert_eq!(validate("").unwrap_err(), "name is empty");
    assert_eq!(
        validate(&"x".repeat(65)).unwrap_err(),
        "name is too long (64 max)"
    );
    assert_eq!(validate("a/b").unwrap_err(), "use letters, digits, - or _");
}

/// The boundary itself, from both sides: 64 is the last name that fits and 65
/// is the first that does not.
#[test]
fn sixty_four_characters_fit_and_sixty_five_do_not() {
    assert_eq!(validate(&"x".repeat(64)), Ok(()));
    assert!(validate(&"x".repeat(65)).is_err());
}

/// Length is counted in characters, not bytes, so a name is not refused as too
/// long for a reason the user cannot see. It is still refused — for its
/// characters, which is the reason they can act on.
#[test]
fn a_multibyte_name_is_refused_for_its_characters_not_its_length() {
    let name = "æ".repeat(40); // 80 bytes, 40 characters
    assert_eq!(validate(&name).unwrap_err(), "use letters, digits, - or _");
}
