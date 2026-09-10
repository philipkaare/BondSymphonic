//! Launches the daemon inside the WSL distro and hands back the port and token it
//! printed. This is the only module that knows about `wsl.exe` and Windows path
//! shapes; like `model` and `client`, it must never import Qt types.

use anyhow::{anyhow, Context, Result};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub distro: String,
    pub daemon_path_in_wsl: String,
    /// Windows-side path of a freshly built Linux daemon binary; if set and its content
    /// differs from the installed one, it is copied into the distro first.
    pub local_daemon_binary: Option<PathBuf>,
    pub log_level: String,
}

pub struct DaemonProcess {
    pub child: Child,
    /// Kept open for the daemon's lifetime: closing it is how the IDE asks the
    /// daemon to exit, so dropping a `DaemonProcess` also shuts the daemon down.
    pub stdin: Option<ChildStdin>,
    pub port: u16,
    pub token: String,
}

/// The file name the Linux daemon binary carries on both sides. No `.exe`: it
/// is an ELF that only ever runs inside the distro, even while it sits on an
/// NTFS volume next to a Windows executable.
pub const DAEMON_FILE_NAME: &str = "bondsymphonic-daemon";

/// Overrides which daemon binary this IDE installs into the distro. Used by
/// tests, and by anyone who wants a package to run a daemon built elsewhere.
pub const DAEMON_BINARY_ENV: &str = "BS_DAEMON_BINARY";

/// The daemon's exit code for "another daemon already owns this data
/// directory" (`BUSY_EXIT_CODE` in `crates/daemon/src/daemon.rs`). It has a
/// code of its own precisely so the IDE can tell it from an ordinary start-up
/// failure: relaunching is the one answer that cannot work.
pub const DAEMON_BUSY_EXIT_CODE: i32 = 2;

/// The data directory the daemon uses, for the message the IDE shows when
/// another daemon already owns it. [`command_for`] passes no `--data-dir`, so
/// this is the daemon's own default and must track it.
pub const DAEMON_DATA_DIR: &str = "~/.bondsymphonic";

/// The daemon closed stdout without ever printing its port line, which means it
/// exited while starting up. `code` is the exit code when one could be read.
///
/// Its own error type rather than a sentence, because the exit code decides
/// what the IDE does next: code 2 is a data directory another daemon owns and
/// no relaunch can change that, while every other exit is worth trying again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonExited {
    pub code: Option<i32>,
}

impl std::fmt::Display for DaemonExited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the daemon closed stdout before printing its port")?;
        match self.code {
            Some(code) => write!(f, "; it exited with code {code}"),
            None => Ok(()),
        }
    }
}

impl std::error::Error for DaemonExited {}

/// The Linux daemon binary this build of the IDE ships with, or `None` when
/// there is none to install and whatever is already in the distro must do.
///
/// See [`resolve_daemon_binary`] for the order.
pub fn local_daemon_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    resolve_daemon_binary(&exe, std::env::var_os(DAEMON_BINARY_ENV).as_deref())
}

/// The resolution order, split out from [`local_daemon_binary`] so it is
/// testable without a real executable or a mutated environment:
///
/// 1. `BS_DAEMON_BINARY`, verbatim. An override that names a file which is not
///    there is still the answer: a caller who set it meant that path, and
///    quietly running some other daemon instead is how a test ends up proving
///    nothing. `install_daemon` skips a missing binary anyway, so the outcome
///    is "nothing installed", not a crash.
/// 2. `<exe dir>\bondsymphonic-daemon` — the packaged layout `package.ps1`
///    builds, where the daemon sits beside the exe in `dist\BondSymphonic\`.
/// 3. `<exe dir>\..\daemon\bondsymphonic-daemon` — the development tree, where
///    the exe is in `target\<profile>\` and `build-daemon.ps1` leaves the
///    daemon in `target\daemon\`.
///
/// Packaged before development, so a package unzipped inside a checkout runs
/// the daemon it shipped with rather than whatever the checkout last built.
fn resolve_daemon_binary(exe: &Path, override_path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if let Some(raw) = override_path {
        // An empty variable is how a shell spells "unset"; `PathBuf::from("")`
        // would otherwise become a relative path naming the current directory.
        if !raw.is_empty() {
            let path = PathBuf::from(raw);
            if !path.exists() {
                tracing::warn!(
                    "{DAEMON_BINARY_ENV}={} does not exist; no daemon will be installed",
                    path.display()
                );
            }
            return Some(path);
        }
    }
    let dir = exe.parent()?;
    let packaged = dir.join(DAEMON_FILE_NAME);
    if packaged.exists() {
        return Some(packaged);
    }
    // `?` on the grandparent only after the packaged candidate has been tried,
    // so an exe sitting at the root of a volume still finds its own daemon.
    let dev = dir.parent()?.join("daemon").join(DAEMON_FILE_NAME);
    dev.exists().then_some(dev)
}

/// Test hook: a loopback `host:port` to connect to instead of starting a daemon.
pub const TEST_ADDR_ENV: &str = "BS_DAEMON_ADDR";
/// Test hook: the handshake token used with [`TEST_ADDR_ENV`].
pub const TEST_TOKEN_ENV: &str = "BS_DAEMON_TOKEN";

/// **Test-only.** The daemon endpoint named in the environment, if there is one.
///
/// With `BS_DAEMON_ADDR` set to a loopback `host:port`, the IDE connects
/// straight to that address using the token in `BS_DAEMON_TOKEN` and never runs
/// `wsl.exe`. It exists so `tests/smoke.rs` can run the real IDE binary against
/// an in-process fake daemon. Every ordinary run leaves the variable unset, this
/// returns `None`, and the launch path is exactly what it would be without the
/// hook.
///
/// A rejected address is reported and ignored rather than guessed at, so a typo
/// does not silently become a normal WSL launch failure.
pub fn test_endpoint() -> Option<(SocketAddr, String)> {
    let raw = std::env::var(TEST_ADDR_ENV).ok()?;
    match parse_endpoint(&raw) {
        Ok(addr) => Some((addr, std::env::var(TEST_TOKEN_ENV).unwrap_or_default())),
        Err(why) => {
            tracing::error!("{TEST_ADDR_ENV}={raw:?} {why}; ignoring the test hook");
            None
        }
    }
}

/// The parsing half of [`test_endpoint`], split out so it is testable without
/// mutating the process environment. The error is the reason, for the log.
///
/// Loopback only. The hook's whole purpose is a fake daemon in the same test
/// run, and the daemon protocol carries file listings, file writes and PTY
/// traffic, so an off-host address is never what was meant.
fn parse_endpoint(raw: &str) -> Result<SocketAddr, &'static str> {
    let addr: SocketAddr = raw.trim().parse().map_err(|_| "is not a host:port")?;
    if !addr.ip().is_loopback() {
        return Err("is not a loopback address");
    }
    Ok(addr)
}

/// Reads the daemon's first stdout line, `{"port":N,"token":"..."}`.
pub fn parse_port_line(line: &str) -> Option<(u16, String)> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let port = u16::try_from(v.get("port")?.as_u64()?).ok()?;
    let token = v.get("token")?.as_str()?.to_string();
    Some((port, token))
}

/// `C:\git\x` becomes `/mnt/c/git/x`. Paths that are already POSIX are passed through.
pub fn windows_path_to_wsl(p: &Path) -> Option<String> {
    let s = p.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        return Some(s);
    }
    let mut chars = s.chars();
    let drive = chars.next()?.to_ascii_lowercase();
    if !drive.is_ascii_alphabetic() || chars.next()? != ':' {
        return None;
    }
    let rest: String = chars.collect();
    Some(format!("/mnt/{drive}{rest}"))
}

/// The shell wrapper lets `~` expand and picks up the user's PATH from .bashrc.
pub fn command_for(spec: &LaunchSpec) -> (String, Vec<String>) {
    let inner = format!(
        "exec {} --log-level {}",
        spec.daemon_path_in_wsl, spec.log_level
    );
    if cfg!(windows) {
        (
            "wsl.exe".into(),
            vec![
                "-d".into(),
                spec.distro.clone(),
                "--".into(),
                "bash".into(),
                "-lc".into(),
                inner,
            ],
        )
    } else {
        ("bash".into(), vec!["-lc".into(), inner])
    }
}

/// Runs a one-shot shell script inside the distro and returns its trimmed stdout.
/// Everything goes through `bash -lc`, so stdout is the script's own UTF-8 bytes
/// rather than the UTF-16 that `wsl.exe` uses for its own diagnostics.
async fn wsl(spec: &LaunchSpec, script: &str) -> Result<String> {
    let out = if cfg!(windows) {
        Command::new("wsl.exe")
            .args(["-d", &spec.distro, "--", "bash", "-lc", script])
            .output()
            .await?
    } else {
        Command::new("bash").args(["-lc", script]).output().await?
    };
    if !out.status.success() {
        return Err(anyhow!(
            "wsl command failed: {}\n{}",
            script,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Renders a distro-side path as one shell word for the scripts sent through `wsl.exe`.
///
/// Single quotes, not double: the whole script travels as a single argv element, and the
/// backslash escaping Rust applies to embedded double quotes does not survive `wsl.exe`'s
/// own command-line splitting, which corrupts the script. A leading `~/` is left outside
/// the quotes so tilde expansion still happens.
fn quoted_distro_path(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => format!("~/'{rest}'"),
        None => format!("'{path}'"),
    }
}

/// Copies the local daemon binary into the distro when the installed copy is missing or
/// holds different content. Comparing `--version` is useless here: every workspace crate
/// stays at 0.1.0 through development, so a rebuilt daemon would never be reinstalled and
/// the IDE would silently keep running a stale binary. The two files are hashed instead.
///
/// Idempotent, and public so a caller can install without launching. [`launch`]
/// runs it before every spawn, which is what makes a protocol mismatch mean
/// something other than a stale binary: the copy in the distro has already been
/// brought level with the one this IDE ships before the handshake is tried.
pub async fn install_daemon(spec: &LaunchSpec) -> Result<()> {
    let Some(local) = &spec.local_daemon_binary else {
        return Ok(());
    };
    if !local.exists() {
        return Ok(());
    }
    // `local` is a Linux ELF binary on a DrvFs mount, so both hashes are taken inside WSL.
    let src = windows_path_to_wsl(local).context("bad local path")?;
    let dst = quoted_distro_path(&spec.daemon_path_in_wsl);
    let tmp = quoted_distro_path(&format!("{}.tmp", spec.daemon_path_in_wsl));
    // The parent directory is derived here rather than with `dirname` in the shell, so
    // the script needs no command substitution and stays one quoting level deep.
    let dst_dir = quoted_distro_path(
        spec.daemon_path_in_wsl
            .rsplit_once('/')
            .map_or(".", |(parent, _)| parent),
    );

    let local_hash = wsl(spec, &format!("sha256sum '{src}' | cut -d' ' -f1"))
        .await
        .unwrap_or_default();
    let installed_hash = wsl(
        spec,
        &format!("sha256sum {dst} 2>/dev/null | cut -d' ' -f1 || true"),
    )
    .await
    .unwrap_or_default();

    let short = |h: &str| h.chars().take(12).collect::<String>();
    if !local_hash.is_empty() && local_hash == installed_hash {
        tracing::info!(hash = %short(&local_hash), "daemon already current in distro");
        return Ok(());
    }

    // Copy to a temporary name and rename over the target: replacing a binary that is
    // currently executing fails with "Text file busy", whereas renaming over it does not.
    wsl(
        spec,
        &format!("mkdir -p {dst_dir} && cp '{src}' {tmp} && chmod +x {tmp} && mv -f {tmp} {dst}"),
    )
    .await?;
    tracing::info!(
        local_hash = %short(&local_hash),
        installed_hash = %short(&installed_hash),
        "installed daemon into distro"
    );
    Ok(())
}

pub async fn launch(spec: &LaunchSpec) -> Result<DaemonProcess> {
    install_daemon(spec).await?;
    let (prog, args) = command_for(spec);
    let mut child = Command::new(&prog)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning {prog}"))?;
    let stdout = child.stdout.take().context("no stdout")?;
    let stderr = child.stderr.take().context("no stderr")?;
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            tracing::info!(target: "daemon", "{l}");
        }
    });
    let mut lines = BufReader::new(stdout).lines();
    let first = tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line())
        .await
        .context("daemon did not print its port within 30s")??;
    let Some(first) = first else {
        // Stdout closed with no port line: the daemon exited during start-up.
        // Reaped here so its exit code travels with the error -- that code is
        // how the caller tells a data directory another daemon owns from a
        // failure another launch could get past.
        return Err(anyhow::Error::new(exit_of(&mut child).await));
    };
    let (port, token) =
        parse_port_line(&first).ok_or_else(|| anyhow!("unexpected daemon first line: {first}"))?;
    let stdin = child.stdin.take();
    Ok(DaemonProcess {
        child,
        stdin,
        port,
        token,
    })
}

/// Waits briefly for a daemon that has already closed stdout and reports how it
/// ended. Bounded, because a `wsl.exe` relay that outlives the daemon it
/// launched must not hold the launch path open for the rest of the session; an
/// exit code that cannot be read in five seconds is reported as none, which the
/// caller treats as an ordinary failure.
async fn exit_of(child: &mut Child) -> DaemonExited {
    let code = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .ok()
        .and_then(|status| status.ok())
        .and_then(|status| status.code());
    DaemonExited { code }
}

impl DaemonProcess {
    /// Resolves when the daemon process ends, whatever ended it.
    ///
    /// The connection dying is the usual way the IDE learns the daemon is
    /// gone, but not the only one: a `wsl.exe` relay can be killed with the
    /// socket left half-open, and a daemon that wedges without closing its
    /// socket is a process the IDE should still notice the death of. The
    /// reconnect loop waits on this alongside the event stream and acts on
    /// whichever answers first.
    ///
    /// Cancel-safe, which the reconnect loop's `select!` relies on: awaiting
    /// this and then dropping the future leaves the child exactly as it was.
    /// It also closes nothing -- [`launch`] moves the child's stdin onto
    /// [`DaemonProcess::stdin`], so the handle tokio would drop here is
    /// already gone and the daemon is not asked to exit by being waited on.
    ///
    /// The full status is returned rather than a bare "it ended", because
    /// `ExitStatus::code()` is what the supervisor reads: exit
    /// [`DAEMON_BUSY_EXIT_CODE`] is a data directory another daemon owns, and
    /// relaunching into it would only produce the same exit again.
    pub async fn wait_exit(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    /// Closes stdin, which is how the daemon is asked to exit, and waits up to 5s
    /// for it to do so before killing the `wsl.exe` relay.
    pub async fn shutdown(mut self) {
        drop(self.stdin.take());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait()).await;
        let _ = self.child.kill().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_port_line() {
        assert_eq!(
            parse_port_line(r#"{"port":41234,"token":"abc"}"#),
            Some((41234, "abc".into()))
        );
        assert_eq!(parse_port_line("garbage"), None);
        assert_eq!(parse_port_line(r#"{"port":"x"}"#), None);
    }

    #[test]
    fn parses_test_hook_endpoints() {
        assert_eq!(
            parse_endpoint("127.0.0.1:41234"),
            Ok(([127, 0, 0, 1], 41234).into())
        );
        // Whitespace is what a shell leaves behind. The whole 127.0.0.0/8 block
        // and IPv6 `::1` are loopback too.
        assert_eq!(
            parse_endpoint("  127.0.0.1:1  "),
            Ok(([127, 0, 0, 1], 1).into())
        );
        assert_eq!(
            parse_endpoint("127.9.9.9:41234"),
            Ok(([127, 9, 9, 9], 41234).into())
        );
        assert!(parse_endpoint("[::1]:41234").is_ok());

        // A bare port or a hostname is not an address and must not be guessed at.
        assert_eq!(parse_endpoint("41234"), Err("is not a host:port"));
        assert_eq!(parse_endpoint("localhost:41234"), Err("is not a host:port"));
        assert_eq!(parse_endpoint(""), Err("is not a host:port"));

        // Parseable but off-host: refused, so the hook can never point the IDE
        // at a daemon on another machine.
        let off_host = "is not a loopback address";
        assert_eq!(parse_endpoint("10.0.0.5:9000"), Err(off_host));
        assert_eq!(parse_endpoint("0.0.0.0:9000"), Err(off_host));
        assert_eq!(parse_endpoint("[2001:db8::1]:9000"), Err(off_host));
    }

    #[test]
    fn converts_windows_paths() {
        assert_eq!(
            windows_path_to_wsl(std::path::Path::new(r"C:\git\Bond")),
            Some("/mnt/c/git/Bond".into())
        );
        assert_eq!(
            windows_path_to_wsl(std::path::Path::new(r"D:\a b\c")),
            Some("/mnt/d/a b/c".into())
        );
        assert_eq!(
            windows_path_to_wsl(std::path::Path::new("/already/posix")),
            Some("/already/posix".into())
        );
    }

    #[test]
    fn quotes_distro_paths_without_double_quotes() {
        // A leading `~/` stays outside the quotes so bash still expands it, and no
        // double quote may appear: wsl.exe corrupts the escaping Rust applies to them.
        assert_eq!(
            quoted_distro_path("~/.bondsymphonic/bin/bondsymphonic-daemon"),
            "~/'.bondsymphonic/bin/bondsymphonic-daemon'"
        );
        assert_eq!(
            quoted_distro_path("/opt/bond symphonic/daemon"),
            "'/opt/bond symphonic/daemon'"
        );
        assert!(!quoted_distro_path("~/a/b").contains('"'));
    }

    /// A throwaway directory of this test's own, with the given tag.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bs-launcher-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent dir");
        }
        std::fs::write(path, b"not really an ELF").expect("fixture binary");
    }

    #[test]
    fn resolves_the_daemon_binary_in_order() {
        // A packaged tree: the exe and the daemon side by side in one folder.
        // A dev tree: `target\<profile>\bondsymphonic-ide.exe` with the daemon
        // one level up in `target\daemon\`, where build-daemon.ps1 leaves it.
        let root = scratch("resolve");
        let packaged_exe = root.join("BondSymphonic").join("bondsymphonic-ide.exe");
        let packaged_daemon = root.join("BondSymphonic").join(DAEMON_FILE_NAME);
        let dev_exe = root
            .join("target")
            .join("release")
            .join("bondsymphonic-ide.exe");
        let dev_daemon = root.join("target").join("daemon").join(DAEMON_FILE_NAME);
        touch(&packaged_exe);
        touch(&dev_exe);

        // Nothing built yet: no candidate exists, and the launcher says so
        // rather than naming a path that is not there.
        assert_eq!(resolve_daemon_binary(&dev_exe, None), None);
        assert_eq!(resolve_daemon_binary(&packaged_exe, None), None);

        // The dev tree only.
        touch(&dev_daemon);
        assert_eq!(
            resolve_daemon_binary(&dev_exe, None),
            Some(dev_daemon.clone())
        );
        // The packaged exe never reaches back into a `target\daemon` of its
        // own: `dist\BondSymphonic\..\daemon` is not a thing that exists.
        assert_eq!(resolve_daemon_binary(&packaged_exe, None), None);

        // Packaged: the copy beside the exe wins, and it is found even though
        // this exe has no `target\daemon` above it.
        touch(&packaged_daemon);
        assert_eq!(
            resolve_daemon_binary(&packaged_exe, None),
            Some(packaged_daemon.clone())
        );

        // A daemon beside the exe outranks the dev tree above it, so a
        // packaged folder unpacked inside a checkout still runs its own copy.
        touch(&dev_exe.with_file_name(DAEMON_FILE_NAME));
        assert_eq!(
            resolve_daemon_binary(&dev_exe, None),
            Some(dev_exe.with_file_name(DAEMON_FILE_NAME))
        );

        // The environment override outranks both, and is honoured verbatim:
        // a test that points it at a path of its own gets that path, never a
        // silent fallback to whatever the build tree happens to contain.
        let elsewhere = root.join("elsewhere").join(DAEMON_FILE_NAME);
        touch(&elsewhere);
        assert_eq!(
            resolve_daemon_binary(&packaged_exe, Some(elsewhere.as_os_str())),
            Some(elsewhere.clone())
        );
        let missing = root.join("gone").join(DAEMON_FILE_NAME);
        assert_eq!(
            resolve_daemon_binary(&packaged_exe, Some(missing.as_os_str())),
            Some(missing)
        );

        // An empty variable is how a shell spells "unset"; it must not become
        // an override naming the current directory.
        assert_eq!(
            resolve_daemon_binary(&packaged_exe, Some(std::ffi::OsStr::new(""))),
            Some(packaged_daemon)
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn builds_wsl_command_line() {
        let spec = LaunchSpec {
            distro: "bondsymphonic".into(),
            daemon_path_in_wsl: "~/.bondsymphonic/bin/bondsymphonic-daemon".into(),
            local_daemon_binary: None,
            log_level: "debug".into(),
        };
        let (prog, args) = command_for(&spec);
        assert_eq!(prog, "wsl.exe");
        assert_eq!(
            args,
            vec![
                "-d",
                "bondsymphonic",
                "--",
                "bash",
                "-lc",
                "exec ~/.bondsymphonic/bin/bondsymphonic-daemon --log-level debug"
            ]
        );
    }
}
