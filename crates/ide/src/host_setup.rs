//! First-run setup of the Linux the daemon runs in: the `bondsymphonic` WSL
//! distro on Windows, the Lima VM of the same name on macOS.
//!
//! An installed IDE sets this up itself, before the first daemon launch,
//! from the provisioning scripts it ships with (`setup-wsl.ps1` and
//! `setup-wsl.sh` -- the same ones `launch.ps1` and `launch.sh` use in a
//! checkout). Every step is idempotent, so the check runs on every start: on
//! a machine that is already set up it costs a couple of quick commands, and
//! on macOS it is also what starts a VM left stopped by a reboot.
//!
//! Blocking, and free of Qt: `qobjects::host_setup` runs it on a blocking
//! thread and turns the [`Reporter`] calls into signals.

use crate::launcher::{distro_shell, single_quoted};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What a run of [`ensure`] ended in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ready,
    /// Windows without a working WSL. Installing it needs elevation and,
    /// usually, a restart, so it is the user's step: see [`install_wsl`].
    NeedsWsl,
    /// WSL was just installed and Windows has to restart before it works.
    NeedsRestart,
    Failed(String),
}

/// Hears what [`ensure`] is doing.
pub trait Reporter {
    /// A step the user should be able to see is starting, in a sentence.
    fn step(&mut self, title: &str);
    /// One line of a command's output.
    fn line(&mut self, line: &str);
}

/// The provisioning scripts and whether they came from a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scripts {
    pub dir: PathBuf,
    /// A development tree: provision with the Rust toolchain too, since the
    /// daemon is built inside the distro there.
    pub dev: bool,
}

/// Where the scripts are for the executable at `exe`: beside it (Windows
/// package), in the app bundle's `Contents/Resources` (macOS), or in the
/// checkout's `scripts/` when `exe` is `target/<profile>/<exe>`.
pub fn find_scripts(exe: &Path) -> Option<Scripts> {
    let dir = exe.parent()?;
    let packaged = crate::launcher::packaged_resource_dir(exe).unwrap_or_else(|| dir.to_path_buf());
    let dev = dir
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("scripts"));
    [(Some(packaged), false), (dev, true)]
        .into_iter()
        .filter_map(|(dir, dev)| dir.map(|dir| Scripts { dir, dev }))
        .find(|s| s.dir.join("setup-wsl.sh").is_file())
}

/// What the readiness check asks of the distro: the tools the daemon cannot
/// start a workspace without.
const READY_PROBE: &str =
    "command -v git >/dev/null && command -v bwrap >/dev/null && command -v python3 >/dev/null";

/// Brings the distro or VM named `distro` to the point a daemon can run in it.
pub fn ensure(distro: &str, scripts: Option<&Scripts>, r: &mut dyn Reporter) -> Outcome {
    let result = if cfg!(windows) {
        windows_ensure(distro, scripts, r)
    } else if cfg!(target_os = "macos") {
        mac_ensure(distro, scripts, r)
    } else {
        // Linux runs the daemon directly; there is nothing to set up.
        Ok(Outcome::Ready)
    };
    result.unwrap_or_else(Outcome::Failed)
}

fn windows_ensure(
    distro: &str,
    scripts: Option<&Scripts>,
    r: &mut dyn Reporter,
) -> Result<Outcome, String> {
    if !quiet_success(Command::new("wsl.exe").arg("--status")) {
        return Ok(Outcome::NeedsWsl);
    }
    if distro_ready(distro) {
        return Ok(Outcome::Ready);
    }
    let scripts = scripts.ok_or("the setup scripts are missing from this installation")?;
    let ps1 = scripts.dir.join("setup-wsl.ps1");
    if !ps1.is_file() {
        return Err(format!("{} is missing", ps1.display()));
    }
    r.step(if wsl_has_distro(distro) {
        "Finishing the setup of the Linux environment…"
    } else {
        "Setting up the Linux environment. This downloads about a gigabyte and takes a few minutes…"
    });
    let mut cmd = Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ])
    .arg(&ps1);
    if !scripts.dev {
        cmd.arg("-Runtime");
    }
    run_streamed(cmd, r)?;
    if distro_ready(distro) {
        Ok(Outcome::Ready)
    } else {
        Err("the setup finished, but the Linux environment is still missing git, bubblewrap or python3".into())
    }
}

/// Installs WSL itself, elevated. Windows shows its own permission prompt;
/// declining it is an error. A restart is usually needed afterwards.
pub fn install_wsl(r: &mut dyn Reporter) -> Result<(), String> {
    r.step("Installing WSL. Windows will ask for permission…");
    let mut cmd = Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "$p = Start-Process -FilePath wsl.exe -ArgumentList '--install','--no-distribution' \
         -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
    ]);
    run_streamed(cmd, r)
}

/// Whether WSL now works -- after [`install_wsl`], it does on some machines
/// without a restart.
pub fn wsl_works() -> bool {
    quiet_success(Command::new("wsl.exe").arg("--status"))
}

/// `wsl.exe -l -q` answers in UTF-16LE.
fn wsl_has_distro(distro: &str) -> bool {
    let mut cmd = Command::new("wsl.exe");
    cmd.args(["-l", "-q"]);
    hide_window(&mut cmd);
    let Ok(out) = cmd.stderr(Stdio::null()).output() else {
        return false;
    };
    decode_wsl_output(&out.stdout)
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case(distro))
}

/// `wsl.exe`'s own output is UTF-16LE; anything else is read as UTF-8.
pub fn decode_wsl_output(bytes: &[u8]) -> String {
    // ASCII text in UTF-16LE has a NUL in every odd byte; UTF-8 has none.
    let utf16 = bytes.len() % 2 == 0 && bytes.iter().skip(1).step_by(2).any(|&b| b == 0);
    if utf16 {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units).replace('\u{feff}', "")
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn mac_ensure(
    vm: &str,
    scripts: Option<&Scripts>,
    r: &mut dyn Reporter,
) -> Result<Outcome, String> {
    let limactl = crate::launcher::limactl();
    let status = {
        let out = Command::new(&limactl)
            .args(["list", "--format", "{{.Status}}", vm])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| {
                format!(
                    "{} could not be run ({e}); is Lima installed?",
                    limactl.display()
                )
            })?;
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    match status.as_str() {
        "Running" => {}
        "" => {
            r.step(
                "Creating the Linux VM. This downloads about a gigabyte and takes a few minutes…",
            );
            let mut create = Command::new(&limactl);
            // The home directory, writable and at the same path: the daemon is
            // installed from it and the user's repositories live in it.
            create.args([
                "create",
                &format!("--name={vm}"),
                "--tty=false",
                "--mount-writable",
                "template://ubuntu-24.04",
            ]);
            run_streamed(create, r)?;
            start_vm(&limactl, vm, r)?;
        }
        _ => start_vm(&limactl, vm, r)?,
    }
    if distro_ready(vm) {
        return Ok(Outcome::Ready);
    }
    let scripts = scripts.ok_or("the setup scripts are missing from this installation")?;
    r.step("Installing git, bubblewrap and the agent tools inside the VM…");
    // The VM sees only the home directory, and the app bundle is not in it.
    let staged = stage_script(&scripts.dir.join("setup-wsl.sh"))?;
    let user = shell_output(vm, "whoami")?;
    let mode = if scripts.dev { "dev" } else { "runtime" };
    let script = format!(
        "sudo bash {} {} {mode}",
        single_quoted(&staged.to_string_lossy()),
        single_quoted(&user)
    );
    let (prog, args) = distro_shell(vm, script);
    let mut cmd = Command::new(prog);
    cmd.args(args);
    run_streamed(cmd, r)?;
    if distro_ready(vm) {
        Ok(Outcome::Ready)
    } else {
        Err("the setup finished, but the VM is still missing git, bubblewrap or python3".into())
    }
}

fn start_vm(limactl: &Path, vm: &str, r: &mut dyn Reporter) -> Result<(), String> {
    r.step("Starting the Linux VM…");
    let mut start = Command::new(limactl);
    start.args(["start", "--tty=false", vm]);
    run_streamed(start, r)
}

/// Copies `script` to `~/Library/Caches/BondSymphonic/provision/`.
fn stage_script(script: &Path) -> Result<PathBuf, String> {
    let dirs = directories::BaseDirs::new().ok_or("there is no home directory")?;
    let dir = dirs.cache_dir().join("BondSymphonic").join("provision");
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let dest = dir.join(script.file_name().unwrap_or_default());
    std::fs::copy(script, &dest).map_err(|e| format!("copying {}: {e}", script.display()))?;
    Ok(dest)
}

fn distro_ready(distro: &str) -> bool {
    let (prog, args) = distro_shell(distro, READY_PROBE.to_owned());
    let mut cmd = Command::new(prog);
    cmd.args(args);
    quiet_success(&mut cmd)
}

fn shell_output(distro: &str, script: &str) -> Result<String, String> {
    let (prog, args) = distro_shell(distro, script.to_owned());
    let mut cmd = Command::new(&prog);
    cmd.args(args).stdin(Stdio::null()).stderr(Stdio::null());
    hide_window(&mut cmd);
    let out = cmd.output().map_err(|e| format!("{prog}: {e}"))?;
    if !out.status.success() {
        return Err(format!("`{script}` failed in {distro}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn quiet_success(cmd: &mut Command) -> bool {
    hide_window(cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A GUI process starting a console program gets a console window flashed up
/// for it on Windows unless it asks otherwise.
fn hide_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// Runs `cmd`, passing each line of its stdout and stderr to `r` as it
/// arrives, and fails unless it exits successfully.
fn run_streamed(mut cmd: Command, r: &mut dyn Reporter) -> Result<(), String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    hide_window(&mut cmd);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program} could not be started: {e}"))?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|stream| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stream);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        for line in clean_lines(&buf) {
                            if tx.send(line).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        })
    })
    .collect();
    drop(tx);
    for line in rx {
        r.line(&line);
    }
    for reader in readers {
        let _ = reader.join();
    }
    let status = child.wait().map_err(|e| format!("{program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} failed ({status}); the output above says why"
        ))
    }
}

/// The printable lines in one chunk of output: NULs (UTF-16 from `wsl.exe`
/// relayed through PowerShell) dropped, carriage-return progress redraws
/// reduced to their last state, empty lines skipped.
pub fn clean_lines(chunk: &[u8]) -> Vec<String> {
    let text: String = String::from_utf8_lossy(chunk)
        .chars()
        .filter(|&c| c != '\0')
        .collect();
    text.split('\n')
        .filter_map(|line| {
            let last = line
                .trim_end_matches('\r')
                .rsplit('\r')
                .next()
                .unwrap_or("");
            let last = last.trim_end();
            (!last.is_empty()).then(|| last.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_wsl_utf16_listings() {
        let bytes: Vec<u8> = "Ubuntu\r\nbondsymphonic\r\n"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let text = decode_wsl_output(&bytes);
        assert!(text.lines().any(|l| l.trim() == "bondsymphonic"));
        assert_eq!(decode_wsl_output(b"plain\n"), "plain\n");
        assert_eq!(decode_wsl_output(b""), "");
    }

    #[test]
    fn cleans_progress_output() {
        assert_eq!(
            clean_lines(b"Downloading 10%\rDownloading 55%\rDownloading 100%\r\n"),
            vec!["Downloading 100%"]
        );
        assert_eq!(clean_lines(b"a\0b\n\n"), vec!["ab"]);
        assert!(clean_lines(b"\r\n").is_empty());
    }

    #[test]
    fn finds_scripts_in_a_checkout() {
        let root = std::env::temp_dir().join(format!("bs-host-setup-{}", std::process::id()));
        let exe = root
            .join("target")
            .join("release")
            .join("bondsymphonic-ide");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("scripts").join("setup-wsl.sh"), "").unwrap();
        let found = find_scripts(&exe).unwrap();
        assert!(found.dev);
        assert_eq!(found.dir, root.join("scripts"));
        // A packaged copy beside the exe wins.
        std::fs::write(exe.parent().unwrap().join("setup-wsl.sh"), "").unwrap();
        let found = find_scripts(&exe).unwrap();
        assert!(!found.dev);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
