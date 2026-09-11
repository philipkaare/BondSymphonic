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

/// The WSL distro this IDE installs its daemon into, and the one a
/// `\\wsl.localhost\<distro>\...` path has to name for [`wsl_path`] to be able
/// to convert it. `Settings::distro` defaults to it.
pub const DEFAULT_DISTRO: &str = "bondsymphonic";

/// How many stdout lines are read while looking for the daemon's port line
/// before the launch is given up on. Generous, because the lines before it are
/// a shell profile's own output and there is no upper bound on how chatty one
/// is; bounded all the same, so a profile that never stops talking fails in a
/// second rather than holding the launch open for the whole timeout.
pub const MAX_PORT_LINES: usize = 20;

/// How long the daemon has to print its port line.
pub const PORT_LINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Reads one line and decodes it with replacement characters, or `None` at the
/// end of the stream.
///
/// Deliberately not `tokio::io::Lines`, which yields `Err(InvalidData)` for a
/// line that is not UTF-8. Every caller here stops reading on an error, so one
/// stray byte in a log line -- and those lines are whatever a tool inside the
/// sandbox wrote, in whatever encoding it used -- would silence the rest of the
/// stream for the life of the process.
async fn next_lossy_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    if reader.read_until(b'\n', &mut buf).await? == 0 {
        return Ok(None);
    }
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// Hands every line of `reader` to `emit` until the stream ends. Used for the
/// daemon's stderr, which is a log the IDE relays and never parses.
async fn drain_lines<R, F>(reader: R, mut emit: F)
where
    R: tokio::io::AsyncRead + Unpin,
    F: FnMut(String),
{
    let mut reader = BufReader::new(reader);
    loop {
        match next_lossy_line(&mut reader).await {
            Ok(Some(line)) => emit(line),
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("the daemon's stderr could not be read: {e}");
                return;
            }
        }
    }
}

/// What looking for the daemon's port line on its stdout came to.
#[derive(Debug, PartialEq, Eq)]
enum PortLine {
    Found {
        port: u16,
        token: String,
    },
    /// Stdout closed without one, which means the daemon exited while starting.
    Closed,
    /// [`MAX_PORT_LINES`] lines went by and none of them was it.
    NotAmongTheFirstLines,
    /// The budget ran out with stdout still open and still silent.
    TimedOut,
}

/// Reads the daemon's stdout looking for `{"port":N,"token":"..."}`.
///
/// Every line is tried, not just the first. A login shell prints its own
/// output before the daemon says a word -- a version manager's banner, an MOTD,
/// anything a `.bashrc` echoes -- and taking the first line as the port line
/// turned that into "unexpected daemon first line" against a daemon that was
/// starting perfectly well. The lines that are not it are relayed to the log,
/// which is where such a banner belongs and where the user can read it if the
/// launch does fail.
async fn find_port_line<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    budget: std::time::Duration,
) -> PortLine {
    let deadline = tokio::time::Instant::now() + budget;
    let mut reader = BufReader::new(reader);
    for _ in 0..MAX_PORT_LINES {
        let line = match tokio::time::timeout_at(deadline, next_lossy_line(&mut reader)).await {
            Err(_elapsed) => return PortLine::TimedOut,
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => return PortLine::Closed,
            Ok(Err(e)) => {
                tracing::warn!("the daemon's stdout could not be read: {e}");
                return PortLine::Closed;
            }
        };
        if let Some((port, token)) = parse_port_line(&line) {
            return PortLine::Found { port, token };
        }
        tracing::info!(target: "daemon", "{line}");
    }
    PortLine::NotAmongTheFirstLines
}

/// Reads the daemon's port line, `{"port":N,"token":"..."}`.
pub fn parse_port_line(line: &str) -> Option<(u16, String)> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let port = u16::try_from(v.get("port")?.as_u64()?).ok()?;
    let token = v.get("token")?.as_str()?.to_string();
    Some((port, token))
}

/// The two spellings Windows gives a path inside a WSL distro's own filesystem.
const WSL_UNC_PREFIXES: [&str; 2] = [r"\\wsl.localhost\", r"\\wsl$\"];

/// `s` without `prefix`, compared without regard to ASCII case. Windows path
/// prefixes are not case sensitive, and `\\WSL$\` is a spelling people type.
fn strip_prefix_ignoring_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// The path `distro` knows a Windows path by: `C:\git\x` is `/mnt/c/git/x`, and
/// `\\wsl.localhost\<distro>\home\bs\x` is `/home/bs/x`. A path that is already
/// POSIX is passed through.
///
/// The UNC form is what File Explorer and every Qt file dialog hand back for a
/// repository kept inside the distro, and it is the shape the plain
/// backslash-to-slash conversion got silently wrong: it left
/// `//wsl.localhost/bondsymphonic/home/bs/x`, which looks like an absolute
/// POSIX path, passed the leading-slash check, and reached the daemon as a
/// directory that does not exist.
///
/// A UNC path naming a *different* distro is an error rather than a guess: its
/// files are not reachable under any path inside ours, and the message names
/// both distros so the user can see which one they picked.
pub fn wsl_path(p: &Path, distro: &str) -> Result<String, String> {
    let raw = p.to_string_lossy();
    // Windows accepts either separator in a UNC prefix, so the comparison is
    // made against a copy that spells them all one way.
    let unc = raw.replace('/', "\\");
    if let Some(rest) = WSL_UNC_PREFIXES
        .iter()
        .find_map(|prefix| strip_prefix_ignoring_case(&unc, prefix))
    {
        let (named, inside) = rest.split_once('\\').unwrap_or((rest, ""));
        if !named.eq_ignore_ascii_case(distro) {
            return Err(format!(
                "{} is inside the WSL distro {named:?}, not {distro:?}",
                p.display()
            ));
        }
        return Ok(format!("/{}", inside.replace('\\', "/")));
    }
    if unc.starts_with("\\\\") {
        return Err(format!(
            "{} is a network path, which the distro cannot reach",
            p.display()
        ));
    }
    let s = raw.replace('\\', "/");
    if s.starts_with('/') {
        return Ok(s);
    }
    let mut chars = s.chars();
    let drive = match chars.next() {
        Some(c) => c.to_ascii_lowercase(),
        None => return Err("an empty path names nothing".to_owned()),
    };
    if !drive.is_ascii_alphabetic() || chars.next() != Some(':') {
        return Err(format!(
            "{} does not start with a drive letter",
            p.display()
        ));
    }
    let rest: String = chars.collect();
    Ok(format!("/mnt/{drive}{rest}"))
}

/// [`wsl_path`] against [`DEFAULT_DISTRO`], for the callers with no
/// [`LaunchSpec`] to hand. The reason a path was refused is logged rather than
/// returned, and the answer is `None`.
pub fn windows_path_to_wsl(p: &Path) -> Option<String> {
    match wsl_path(p, DEFAULT_DISTRO) {
        Ok(path) => Some(path),
        Err(why) => {
            tracing::warn!("{why}");
            None
        }
    }
}

/// The shell wrapper lets `~` expand and picks up the user's PATH from .bashrc.
pub fn command_for(spec: &LaunchSpec) -> (String, Vec<String>) {
    // Both halves quoted. The line travels to `bash -lc` as one string, so a
    // daemon installed under a path with a space in it -- which is what
    // `C:\Program Files` becomes the moment a package is unzipped where Windows
    // suggests -- would otherwise start the first half of its own path with the
    // second half as a stray argument.
    let inner = format!(
        "exec {} --log-level {}",
        quoted_distro_path(&spec.daemon_path_in_wsl),
        single_quoted(&spec.log_level)
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
        Some(rest) => format!("~/{}", single_quoted(rest)),
        None => single_quoted(path),
    }
}

/// One single-quoted shell word. A single quote inside it is closed, escaped
/// and reopened (`'\''`), which is the only way to carry one through single
/// quotes and is why this is not an inline `format!`.
fn single_quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// Copies the local daemon binary into the distro when the installed copy is missing or
/// holds different content. Comparing `--version` is useless here: every workspace crate
/// stays at 0.1.0 through development, so a rebuilt daemon would never be reinstalled and
/// the IDE would silently keep running a stale binary. The two files are hashed instead.
///
/// Idempotent, and run by [`launch`] before every spawn -- which is what makes
/// a protocol mismatch mean something other than a stale binary: the copy in
/// the distro has already been brought level with the one this IDE ships before
/// the handshake is tried. Its only caller lives in this module, so it is
/// `pub(crate)`; the `pub` it used to carry was for the retry that premise
/// made pointless.
pub(crate) async fn install_daemon(spec: &LaunchSpec) -> Result<()> {
    let Some(local) = &spec.local_daemon_binary else {
        return Ok(());
    };
    if !local.exists() {
        return Ok(());
    }
    // `local` is a Linux ELF binary on a DrvFs mount, so both hashes are taken inside WSL.
    let src = wsl_path(local, &spec.distro).map_err(|why| anyhow!(why))?;
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
    tokio::spawn(drain_lines(stderr, |line| {
        tracing::info!(target: "daemon", "{line}");
    }));
    let (port, token) = match find_port_line(stdout, PORT_LINE_TIMEOUT).await {
        PortLine::Found { port, token } => (port, token),
        // Stdout closed with no port line: the daemon exited during start-up.
        // Reaped here so its exit code travels with the error -- that code is
        // how the caller tells a data directory another daemon owns from a
        // failure another launch could get past.
        PortLine::Closed => return Err(anyhow::Error::new(exit_of(&mut child).await)),
        PortLine::TimedOut => {
            return Err(anyhow!(
                "the daemon did not print its port within {}s",
                PORT_LINE_TIMEOUT.as_secs()
            ))
        }
        PortLine::NotAmongTheFirstLines => {
            return Err(anyhow!(
                "the daemon printed {MAX_PORT_LINES} lines without its port line; \
                 something in the distro's shell profile is writing to stdout"
            ))
        }
    };
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

    /// A byte that is not UTF-8 ends the line, not the stream. The old drain
    /// used `Lines`, whose `Err(InvalidData)` ended the `while let` and with it
    /// every daemon log line for the rest of the session.
    #[tokio::test]
    async fn the_stderr_drain_decodes_lossily_and_keeps_reading() {
        let mut seen: Vec<String> = Vec::new();
        drain_lines(&b"ok\n\xff\xfe\n more\n"[..], |line| seen.push(line)).await;
        assert_eq!(seen.len(), 3, "got {seen:?}");
        assert_eq!(seen[0], "ok");
        assert_eq!(seen[1], "\u{fffd}\u{fffd}", "the bad line is still a line");
        assert_eq!(seen[2], " more", "and the stream carries on past it");
    }

    #[tokio::test]
    async fn the_port_line_is_found_past_a_chatty_shell_profile() {
        let stdout = "hello\nnvm: using v20\n{\"port\":41234,\"token\":\"abc\"}\n";
        assert_eq!(
            find_port_line(stdout.as_bytes(), PORT_LINE_TIMEOUT).await,
            PortLine::Found {
                port: 41234,
                token: "abc".into()
            }
        );

        // Stdout closing with no port line is the daemon exiting during
        // start-up, which the caller answers by reading its exit code.
        assert_eq!(
            find_port_line(&b"hello\n"[..], PORT_LINE_TIMEOUT).await,
            PortLine::Closed
        );

        // A profile that never stops talking is given up on after a bounded
        // number of lines rather than read for the whole budget.
        let noise = "hello\n".repeat(MAX_PORT_LINES + 5);
        assert_eq!(
            find_port_line(noise.as_bytes(), PORT_LINE_TIMEOUT).await,
            PortLine::NotAmongTheFirstLines
        );
    }

    /// The budget still bounds the whole search, not each line.
    #[tokio::test]
    async fn a_silent_daemon_runs_out_of_budget() {
        let (client, _server) = tokio::io::duplex(64);
        assert_eq!(
            find_port_line(client, std::time::Duration::from_millis(50)).await,
            PortLine::TimedOut
        );
    }

    /// A repository kept inside the distro is handed to the IDE as a UNC path,
    /// and it has a real path inside the distro -- but only when the UNC names
    /// the distro the daemon is running in.
    #[test]
    fn converts_wsl_unc_paths_for_this_distro_only() {
        let ours = "bondsymphonic";
        assert_eq!(
            wsl_path(
                std::path::Path::new(r"\\wsl.localhost\bondsymphonic\home\bs\repo"),
                ours
            ),
            Ok("/home/bs/repo".to_owned())
        );
        // The older `\\wsl$\` spelling, and the case-insensitivity Windows
        // applies to both the prefix and the distro name.
        assert_eq!(
            wsl_path(
                std::path::Path::new(r"\\wsl$\bondsymphonic\home\bs\repo"),
                ours
            ),
            Ok("/home/bs/repo".to_owned())
        );
        assert_eq!(
            wsl_path(std::path::Path::new(r"\\WSL$\BondSymphonic\home\bs"), ours),
            Ok("/home/bs".to_owned())
        );

        // Another distro's filesystem has no path inside ours, so it is refused
        // and the message names the one that was picked.
        let why = wsl_path(
            std::path::Path::new(r"\\wsl.localhost\ubuntu\home\bs\repo"),
            ours,
        )
        .expect_err("another distro cannot be reached");
        assert!(why.contains("ubuntu"), "{why}");
        assert!(why.contains(ours), "{why}");

        // An ordinary network share is refused for the same reason.
        assert!(wsl_path(std::path::Path::new(r"\\server\share\x"), ours).is_err());
    }

    #[test]
    fn converts_windows_paths() {
        // The UNC form goes through the default-distro wrapper too, which is
        // what `AppController::wslPath` hands a file dialog's answer to.
        assert_eq!(
            windows_path_to_wsl(std::path::Path::new(
                r"\\wsl.localhost\bondsymphonic\home\bs\repo"
            )),
            Some("/home/bs/repo".into())
        );
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
                "exec ~/'.bondsymphonic/bin/bondsymphonic-daemon' --log-level 'debug'"
            ]
        );
    }

    /// Neither half of the launch line may be pasted in bare. A daemon
    /// installed under a path with a space in it -- `C:\Program Files` becomes
    /// one the moment a package is unzipped where Windows suggests -- is two
    /// words to `bash -lc`, and the daemon is started with the first half of
    /// its own path and a stray argument.
    #[test]
    fn quotes_the_daemon_path_and_the_log_level() {
        let spec = LaunchSpec {
            distro: "bondsymphonic".into(),
            daemon_path_in_wsl: "/opt/my dir/daemon".into(),
            local_daemon_binary: None,
            log_level: "debug".into(),
        };
        let (_prog, args) = command_for(&spec);
        assert_eq!(
            args.last().map(String::as_str),
            Some("exec '/opt/my dir/daemon' --log-level 'debug'")
        );
    }
}
