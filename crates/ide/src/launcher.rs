//! Launches the daemon inside the WSL distro and hands back the port and token it
//! printed. This is the only module that knows about `wsl.exe` and Windows path
//! shapes; like `model` and `client`, it must never import Qt types.

use anyhow::{anyhow, Context, Result};
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
pub async fn ensure_installed(spec: &LaunchSpec) -> Result<()> {
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
    ensure_installed(spec).await?;
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
        .context("daemon did not print its port within 30s")??
        .context("daemon closed stdout before printing its port")?;
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

impl DaemonProcess {
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
