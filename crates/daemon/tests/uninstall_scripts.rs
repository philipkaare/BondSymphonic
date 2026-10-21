//! The `uninstall_*` setup actions, run for real against a throwaway home.
//!
//! Each is a script that deletes by path on the developer's machine, outside
//! every sandbox, so what it removes and -- above all -- what it leaves are
//! checked against the layout each installer actually makes.
#![cfg(unix)]

use bondsymphonic_daemon::setup::setup_argv;
use bondsymphonic_proto::SetupAction;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Runs `action`'s command with `home` as `$HOME`, and its output.
fn run(action: SetupAction, home: &Path) -> String {
    let argv = setup_argv(action);
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .env("HOME", home)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{action:?} failed: {text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    text
}

/// What `claude.ai/install.sh` leaves: a link in `~/.local/bin` into the
/// versions under `~/.local/share/claude`. Both go; the login stays.
#[test]
fn uninstalling_claude_removes_the_native_install_and_keeps_the_login() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let version = h.join(".local/share/claude/versions/2.1.0");
    write(&version, "binary");
    std::fs::create_dir_all(h.join(".local/bin")).unwrap();
    symlink(&version, h.join(".local/bin/claude")).unwrap();
    write(&h.join(".claude/.credentials.json"), "{}");
    write(&h.join(".claude.json"), "{}");
    write(&h.join(".local/bin/other-tool"), "x");

    let said = run(SetupAction::UninstallClaude, h);
    assert!(said.contains("Removed"), "{said}");
    assert!(!h.join(".local/bin/claude").exists());
    assert!(std::fs::symlink_metadata(h.join(".local/bin/claude")).is_err());
    assert!(!h.join(".local/share/claude").exists());
    assert!(h.join(".claude/.credentials.json").exists());
    assert!(h.join(".claude.json").exists());
    assert!(h.join(".local/bin/other-tool").exists());

    // Again, with nothing left: it says so rather than failing.
    let said = run(SetupAction::UninstallClaude, h);
    assert!(said.contains("nothing was removed"), "{said}");
}

/// A `claude` in `~/.local/bin` that is not the installer's link is someone
/// else's, and stays.
#[test]
fn uninstalling_claude_leaves_a_claude_it_did_not_make() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    write(&h.join(".local/bin/claude"), "#!/bin/sh\n");
    let said = run(SetupAction::UninstallClaude, h);
    assert!(said.contains("Left"), "{said}");
    assert!(h.join(".local/bin/claude").exists());
}

/// What `install-codex.sh` leaves: two links into a version directory under
/// `~/.local/share/bondsymphonic/codex`. All of it goes; the sign-in stays.
#[test]
fn uninstalling_codex_removes_both_links_and_every_version() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    let share = h.join(".local/share/bondsymphonic/codex");
    for version in ["0.1.0", "0.2.0"] {
        write(&share.join(version).join("codex"), "x");
        write(&share.join(version).join("codex-code-mode-host"), "x");
    }
    std::fs::create_dir_all(h.join(".local/bin")).unwrap();
    for name in ["codex", "codex-code-mode-host"] {
        symlink(
            share.join("0.2.0").join(name),
            h.join(".local/bin").join(name),
        )
        .unwrap();
    }
    write(&h.join(".bondsymphonic/codex-home/auth.json"), "{}");
    write(&h.join(".local/share/bondsymphonic/other/keep"), "x");

    run(SetupAction::UninstallCodex, h);
    for name in ["codex", "codex-code-mode-host"] {
        assert!(std::fs::symlink_metadata(h.join(".local/bin").join(name)).is_err());
    }
    assert!(!share.exists());
    assert!(h.join(".bondsymphonic/codex-home/auth.json").exists());
    assert!(h.join(".local/share/bondsymphonic/other/keep").exists());
}

/// What `install-opencode.sh` leaves: its copy in `~/.local/bin`, and the
/// upstream installer's identical one in `~/.opencode/bin`, which the daemon
/// would otherwise still find. Both go; sign-in and configuration stay.
#[test]
fn uninstalling_opencode_removes_its_copy_and_the_identical_upstream_one() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    write(&h.join(".local/bin/opencode"), "opencode 1.14.48");
    write(&h.join(".opencode/bin/opencode"), "opencode 1.14.48");
    write(&h.join(".local/share/opencode/auth.json"), "{}");
    write(&h.join(".config/opencode/opencode.json"), "{}");

    run(SetupAction::UninstallOpencode, h);
    assert!(!h.join(".local/bin/opencode").exists());
    assert!(!h.join(".opencode").exists());
    assert!(h.join(".local/share/opencode/auth.json").exists());
    assert!(h.join(".config/opencode/opencode.json").exists());
}

/// An upstream OpenCode that is not the file this install copied is the
/// user's own, and stays -- and the output says it is still in use.
#[test]
fn uninstalling_opencode_leaves_a_different_upstream_install() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    write(&h.join(".local/bin/opencode"), "opencode 1.14.48");
    write(&h.join(".opencode/bin/opencode"), "opencode 1.20.0");

    let said = run(SetupAction::UninstallOpencode, h);
    assert!(!h.join(".local/bin/opencode").exists());
    assert!(h.join(".opencode/bin/opencode").exists());
    assert!(said.contains("still used"), "{said}");
}
