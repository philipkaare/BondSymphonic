//! Putting a verified download in place.
//!
//! Neither platform can replace a running program's files from inside it, so
//! both hand over to a small helper that waits for this process to exit, does
//! the replacement, and optionally starts the new version:
//!
//! - **Windows**: the download is the Inno Setup installer. A hidden
//!   PowerShell waits for the IDE's PID, then runs it with `/VERYSILENT` into
//!   the same per-user install. Inno's own `/relaunch=1` parameter (see
//!   `installer/windows/BondSymphonic.iss`) restarts the IDE afterwards.
//! - **macOS**: the download is a zip of the signed, notarized app bundle. It
//!   is extracted and its code signature checked against the running bundle's
//!   Team ID *before* the IDE exits, so the helper -- a `/bin/sh` loop -- only
//!   has to swap two directories and `open` the result.

use std::path::{Path, PathBuf};

/// How this copy of the IDE was installed, which decides whether and how it
/// can update itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// Installed by the Inno Setup installer into `dir`.
    WindowsInstaller { dir: PathBuf },
    /// Running from the app bundle at `bundle`.
    MacBundle { bundle: PathBuf },
    /// A development build, an unzipped package, or a platform with no
    /// installer. Updates are checked for but never applied.
    NotInstalled,
}

impl InstallKind {
    /// Works out the kind from the running executable's path.
    pub fn of_exe(exe: &Path) -> Self {
        if cfg!(windows) {
            // Inno writes `unins000.exe` beside the files it installed; nothing
            // else puts one there.
            if let Some(dir) = exe.parent() {
                if dir.join("unins000.exe").is_file() {
                    return Self::WindowsInstaller {
                        dir: dir.to_path_buf(),
                    };
                }
            }
            return Self::NotInstalled;
        }
        if cfg!(target_os = "macos") {
            if let Some(bundle) = bundle_of(exe) {
                return Self::MacBundle { bundle };
            }
        }
        Self::NotInstalled
    }

    pub fn can_install(&self) -> bool {
        !matches!(self, Self::NotInstalled)
    }
}

/// `/x/BondSymphonic.app` for `/x/BondSymphonic.app/Contents/MacOS/<exe>`.
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    let is_bundle = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app";
    is_bundle.then(|| bundle.to_path_buf())
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error(
        "this copy of BondSymphonic was not installed by its installer, so it cannot update itself"
    )]
    NotInstalled,
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Refused(String),
}

/// A download made ready to install: on Windows the installer itself, on
/// macOS the extracted and checked app bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub path: PathBuf,
}

/// Readies `download` for [`install_after_exit`]. On macOS this is the slow
/// part -- extraction and a full code-signature check -- so it runs while the
/// IDE is still open; anything wrong is found here, not after the IDE has gone.
pub fn prepare(kind: &InstallKind, download: &Path, work: &Path) -> Result<Prepared, InstallError> {
    match kind {
        InstallKind::NotInstalled => Err(InstallError::NotInstalled),
        InstallKind::WindowsInstaller { .. } => Ok(Prepared {
            path: download.to_path_buf(),
        }),
        InstallKind::MacBundle { bundle } => mac::prepare(bundle, download, work),
    }
}

/// Starts the helper that waits for process `pid` to exit and then installs
/// `prepared`, starting the new version afterwards if `relaunch`.
///
/// The helper is detached: it outlives this process, which is the point.
/// `log` is where the installer writes what it did, for a support session
/// asking why an update did not take.
pub fn install_after_exit(
    kind: &InstallKind,
    prepared: &Prepared,
    pid: u32,
    relaunch: bool,
    log: &Path,
) -> Result<(), InstallError> {
    match kind {
        InstallKind::NotInstalled => Err(InstallError::NotInstalled),
        InstallKind::WindowsInstaller { .. } => windows::spawn(&prepared.path, pid, relaunch, log),
        InstallKind::MacBundle { bundle } => mac::spawn(&prepared.path, bundle, pid, relaunch, log),
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
/// One PowerShell single-quoted string: `'` doubled, which is the only escape
/// PowerShell has inside single quotes.
pub(crate) fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg_attr(not(windows), allow(dead_code))]
/// The PowerShell the Windows helper runs. Split out so it can be read in a
/// test on any platform.
pub(crate) fn windows_script(installer: &Path, pid: u32, relaunch: bool, log: &Path) -> String {
    // Wait-Process gives up after ten minutes; an IDE still running then has
    // refused to exit, and installing over it would only fail half way.
    format!(
        "$ErrorActionPreference = 'SilentlyContinue'; \
         Wait-Process -Id {pid} -Timeout 600; \
         if (Get-Process -Id {pid}) {{ exit 1 }}; \
         Start-Process -FilePath {installer} -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', {relaunch}, {log})",
        installer = ps_quote(&installer.to_string_lossy()),
        relaunch = ps_quote(if relaunch { "/relaunch=1" } else { "/relaunch=0" }),
        log = ps_quote(&format!("/LOG=\"{}\"", log.to_string_lossy())),
    )
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    pub fn spawn(
        installer: &Path,
        pid: u32,
        relaunch: bool,
        log: &Path,
    ) -> Result<(), InstallError> {
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
            ])
            .arg(windows_script(installer, pid, relaunch, log))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn()?;
        Ok(())
    }
}

#[cfg(not(windows))]
mod windows {
    use super::*;

    pub fn spawn(_: &Path, _: u32, _: bool, _: &Path) -> Result<(), InstallError> {
        Err(InstallError::NotInstalled)
    }
}

/// The `/bin/sh` script the macOS helper runs, with `$1`..`$4` being the PID
/// to wait for, the new bundle, the installed bundle and `1` to relaunch.
///
/// The old bundle is moved aside rather than deleted first, so a failed move
/// of the new one puts it straight back and the user is never left with no
/// app at all.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) const MAC_SCRIPT: &str = r#"
pid="$1"; new="$2"; dst="$3"; relaunch="$4"
i=0
while kill -0 "$pid" 2>/dev/null; do
  i=$((i + 1))
  [ "$i" -gt 1200 ] && { echo "the IDE did not exit; not updating"; exit 1; }
  sleep 0.5
done
old="$dst.updating-$$"
if mv "$dst" "$old"; then
  if mv "$new" "$dst"; then
    rm -rf "$old"
    echo "updated $dst"
  else
    mv "$old" "$dst"
    echo "could not move the new bundle into place; kept the old one"
  fi
else
  echo "could not move $dst aside; not updating"
fi
[ "$relaunch" = 1 ] && open "$dst"
exit 0
"#;

mod mac {
    use super::*;
    use std::process::{Command, Stdio};

    /// The Team ID line of `codesign -dv` (which writes to stderr).
    pub(crate) fn team_id(codesign_dv_stderr: &str) -> Option<String> {
        codesign_dv_stderr
            .lines()
            .find_map(|l| l.strip_prefix("TeamIdentifier="))
            .map(str::trim)
            .filter(|t| !t.is_empty() && *t != "not set")
            .map(str::to_owned)
    }

    fn signing_team(bundle: &Path) -> Result<String, InstallError> {
        let out = Command::new("/usr/bin/codesign")
            .args(["-dv", "--verbose=2"])
            .arg(bundle)
            .output()?;
        team_id(&String::from_utf8_lossy(&out.stderr)).ok_or_else(|| {
            InstallError::Refused(format!(
                "{} carries no Developer ID signature",
                bundle.display()
            ))
        })
    }

    fn run(cmd: &mut Command, what: &str) -> Result<(), InstallError> {
        let out = cmd.output()?;
        if out.status.success() {
            return Ok(());
        }
        Err(InstallError::Refused(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }

    pub fn prepare(bundle: &Path, zip: &Path, work: &Path) -> Result<Prepared, InstallError> {
        // Writable, or the swap would fail after the IDE had already exited.
        let parent = bundle
            .parent()
            .ok_or_else(|| InstallError::Refused("the app bundle has no parent folder".into()))?;
        let probe = parent.join(format!(".bondsymphonic-write-test-{}", std::process::id()));
        std::fs::write(&probe, b"").map_err(|e| {
            InstallError::Refused(format!(
                "{} is not writable ({e}); install the update by downloading it",
                parent.display()
            ))
        })?;
        let _ = std::fs::remove_file(&probe);

        if work.exists() {
            std::fs::remove_dir_all(work)?;
        }
        std::fs::create_dir_all(work)?;
        run(
            Command::new("/usr/bin/ditto")
                .args(["-x", "-k"])
                .arg(zip)
                .arg(work),
            "extracting the update",
        )?;
        let name = bundle.file_name().unwrap_or_default();
        let mut new = work.join(name);
        if !new.is_dir() {
            // The release names its bundle BondSymphonic.app; a user may have
            // renamed theirs.
            new = work.join("BondSymphonic.app");
        }
        if !new.is_dir() {
            return Err(InstallError::Refused(
                "the update holds no app bundle".into(),
            ));
        }
        run(
            Command::new("/usr/bin/codesign")
                .args(["--verify", "--deep", "--strict"])
                .arg(&new),
            "checking the update's code signature",
        )?;
        let ours = signing_team(bundle)?;
        let theirs = signing_team(&new)?;
        if ours != theirs {
            return Err(InstallError::Refused(format!(
                "the update is signed by team {theirs}, this app by {ours}"
            )));
        }
        Ok(Prepared { path: new })
    }

    #[cfg(unix)]
    pub fn spawn(
        new: &Path,
        bundle: &Path,
        pid: u32,
        relaunch: bool,
        log: &Path,
    ) -> Result<(), InstallError> {
        use std::os::unix::process::CommandExt;
        let log = std::fs::File::create(log)?;
        Command::new("/bin/sh")
            .arg("-c")
            .arg(MAC_SCRIPT)
            .arg("bondsymphonic-update")
            .arg(pid.to_string())
            .arg(new)
            .arg(bundle)
            .arg(if relaunch { "1" } else { "0" })
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            // Its own process group, so the terminal or launcher that sends the
            // IDE's group a signal on exit does not take the helper with it.
            .process_group(0)
            .spawn()?;
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn spawn(_: &Path, _: &Path, _: u32, _: bool, _: &Path) -> Result<(), InstallError> {
        Err(InstallError::NotInstalled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_bundle_around_an_executable() {
        assert_eq!(
            bundle_of(Path::new(
                "/Applications/BondSymphonic.app/Contents/MacOS/bondsymphonic-ide"
            )),
            Some(PathBuf::from("/Applications/BondSymphonic.app"))
        );
        assert_eq!(
            bundle_of(Path::new("/repo/target/release/bondsymphonic-ide")),
            None
        );
        assert_eq!(bundle_of(Path::new("/x/Contents/MacOS/ide")), None);
    }

    #[test]
    fn a_dev_build_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let kind = InstallKind::of_exe(&dir.path().join("bondsymphonic-ide"));
        assert_eq!(kind, InstallKind::NotInstalled);
        assert!(!kind.can_install());
    }

    #[test]
    fn reads_the_team_id_from_codesign_output() {
        let out = "Executable=/A.app/Contents/MacOS/x\nAuthority=Developer ID Application: X (ABCDE12345)\nTeamIdentifier=ABCDE12345\n";
        assert_eq!(mac::team_id(out).as_deref(), Some("ABCDE12345"));
        assert_eq!(mac::team_id("TeamIdentifier=not set\n"), None);
        assert_eq!(mac::team_id("Signature=adhoc\n"), None);
    }

    #[test]
    fn quotes_powershell_strings() {
        assert_eq!(
            ps_quote(r"C:\Users\O'Neil\x.exe"),
            r"'C:\Users\O''Neil\x.exe'"
        );
    }

    #[test]
    fn the_windows_helper_waits_then_runs_the_installer_silently() {
        let s = windows_script(
            Path::new(r"C:\Users\a\AppData\Local\BondSymphonic\updates\setup.exe"),
            4242,
            true,
            Path::new(r"C:\logs\install.log"),
        );
        let wait = s.find("Wait-Process -Id 4242").unwrap();
        let start = s.find("Start-Process").unwrap();
        assert!(wait < start);
        assert!(s.contains("'/VERYSILENT'"));
        assert!(s.contains("'/relaunch=1'"));
        assert!(s.contains(r#"'/LOG="C:\logs\install.log"'"#));
        assert!(!windows_script(Path::new("x"), 1, false, Path::new("l")).contains("relaunch=1"));
    }

    /// Runs the real macOS helper script against two directories standing in
    /// for bundles, with a PID that has already exited.
    #[cfg(unix)]
    #[test]
    fn the_mac_helper_swaps_the_bundles() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("App.app");
        let new = dir.path().join("staged").join("App.app");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("old"), "").unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("new"), "").unwrap();
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(MAC_SCRIPT)
            .arg("t")
            .arg(pid.to_string())
            .arg(&new)
            .arg(&dst)
            .arg("0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(dst.join("new").exists());
        assert!(!dst.join("old").exists());
        assert!(!new.exists());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().contains("updating"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
