//! Startup-only managed harness updates. Candidates are checked before an atomic
//! manifest switches the application; existing installs are never overwritten.
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    Claude,
    Codex,
    #[serde(rename = "opencode")]
    OpenCode,
}
impl Harness {
    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }
    fn override_name(self) -> &'static str {
        match self {
            Self::Claude => "BS_CLAUDE_BIN",
            Self::Codex => "BS_CODEX_BIN",
            Self::OpenCode => "BS_OPENCODE_BIN",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub version: String,
    #[serde(default)]
    pub binary: PathBuf,
    #[serde(default)]
    pub helper: Option<PathBuf>,
    #[serde(default)]
    pub unchanged: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpdateEvent {
    pub harness: String,
    pub status: String,
    pub from_version: Option<String>,
    pub to_version: Option<String>,
    pub message: String,
}
#[async_trait]
pub trait UpdateSource: Send + Sync {
    fn installed(&self, harness: Harness) -> Option<PathBuf>;
    async fn version(&self, binary: &Path) -> Result<String>;
    async fn stage(
        &self,
        harness: Harness,
        directory: &Path,
        current_version: Option<&str>,
    ) -> Result<Candidate>;
    async fn compatible(&self, harness: Harness, candidate: &Candidate) -> Result<()>;
}
pub fn managed_root() -> PathBuf {
    crate::setup::host_home().join(".local/share/bondsymphonic/harnesses")
}
static STARTUP_BINARIES: std::sync::OnceLock<[Option<PathBuf>; 3]> = std::sync::OnceLock::new();
pub fn freeze_managed_binaries() {
    STARTUP_BINARIES.get_or_init(|| {
        [Harness::Claude, Harness::Codex, Harness::OpenCode]
            .map(|h| managed_binary(&managed_root(), h))
    });
}
pub fn selected_binary(harness: Harness) -> Option<PathBuf> {
    STARTUP_BINARIES.get().map_or_else(
        || managed_binary(&managed_root(), harness),
        |b| b[harness as usize].clone(),
    )
}
fn release_version(version: &str) -> Option<[u64; 3]> {
    let numbers: Vec<_> = version
        .split('.')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    numbers.try_into().ok()
}
pub fn managed_binary(root: &Path, harness: Harness) -> Option<PathBuf> {
    let root = root.join(harness.name()).canonicalize().ok()?;
    let candidate: Candidate =
        serde_json::from_slice(&std::fs::read(root.join("current.json")).ok()?).ok()?;
    let binary = candidate.binary.canonicalize().ok()?;
    if !binary.starts_with(root.join("releases")) || !binary.is_file() {
        return None;
    }
    if harness == Harness::Codex {
        let helper = candidate.helper?.canonicalize().ok()?;
        if helper != binary.with_file_name("codex-code-mode-host") || !helper.is_file() {
            return None;
        }
    }
    Some(binary)
}

struct Stage {
    path: PathBuf,
    root: PathBuf,
}
impl Stage {
    fn new(root: &Path, harness: Harness) -> Result<Self> {
        let root = root.join(harness.name());
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let path = root.join(format!(".stage-{}", rand::random::<u64>()));
        std::fs::create_dir(&path)?;
        Ok(Self { path, root })
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        // Only the exact directory this update created, verified under its root.
        if let Ok(path) = self.path.canonicalize() {
            if path.starts_with(&self.root) && path != self.root {
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
}
fn validate_candidate(harness: Harness, stage: &Path, candidate: &Candidate) -> Result<()> {
    anyhow::ensure!(
        release_version(&candidate.version).is_some(),
        "release version is not a stable numeric version"
    );
    let stage = stage.canonicalize()?;
    let binary = candidate.binary.canonicalize()?;
    anyhow::ensure!(
        binary.starts_with(&stage) && binary.is_file(),
        "candidate executable escapes staging directory"
    );
    anyhow::ensure!(
        binary.file_name().is_some_and(|n| n == harness.name()),
        "unexpected candidate executable name"
    );
    if harness == Harness::Codex {
        let helper = candidate
            .helper
            .as_ref()
            .ok_or_else(|| anyhow!("Codex release has no matching helper"))?
            .canonicalize()?;
        anyhow::ensure!(
            helper == binary.with_file_name("codex-code-mode-host") && helper.is_file(),
            "Codex helper does not belong to candidate bundle"
        );
    }
    Ok(())
}
fn promote(harness: Harness, stage: &Stage, candidate: &Candidate) -> Result<()> {
    validate_candidate(harness, &stage.path, candidate)?;
    let binary = candidate
        .binary
        .canonicalize()?
        .strip_prefix(stage.path.canonicalize()?)?
        .to_owned();
    let helper = candidate
        .helper
        .as_ref()
        .map(|p| p.canonicalize())
        .transpose()?
        .map(|p| {
            p.strip_prefix(stage.path.canonicalize().unwrap())
                .map(Path::to_owned)
        })
        .transpose()?;
    let releases = stage.root.join("releases");
    std::fs::create_dir_all(&releases)?;
    let destination = releases.join(format!("{}-{}", candidate.version, rand::random::<u64>()));
    std::fs::rename(&stage.path, &destination)?;
    let manifest = Candidate {
        version: candidate.version.clone(),
        binary: destination.join(binary),
        helper: helper.map(|h| destination.join(h)),
        unchanged: false,
    };
    crate::util::atomic::write_atomic(
        &stage.root.join("current.json"),
        &serde_json::to_vec(&manifest)?,
    )?;
    Ok(())
}
pub async fn update_installed(
    root: &Path,
    source: &dyn UpdateSource,
    budget: Duration,
    mut emit: impl FnMut(UpdateEvent),
) -> Vec<UpdateEvent> {
    let mut report = Vec::new();
    for harness in [Harness::Claude, Harness::Codex, Harness::OpenCode] {
        let mut event = UpdateEvent {
            harness: harness.name().into(),
            status: "checking".into(),
            from_version: None,
            to_version: None,
            message: "Checking installed harness".into(),
        };
        emit(event.clone());
        let Some(installed) = source.installed(harness) else {
            event.status = "skipped".into();
            event.message = "Not installed or supplied by an explicit executable override".into();
            emit(event.clone());
            report.push(event);
            continue;
        };
        let result = tokio::time::timeout(budget, async {
            event.from_version = source.version(&installed).await.ok();
            event.status = "updating".into();
            event.message = "Downloading and checking the newest stable release".into();
            emit(event.clone());
            let stage = Stage::new(root, harness)?;
            let candidate = source
                .stage(harness, &stage.path, event.from_version.as_deref())
                .await?;
            event.to_version = Some(candidate.version.clone());
            let newest = release_version(&candidate.version)
                .ok_or_else(|| anyhow!("invalid stable release version"))?;
            let current = event.from_version.as_deref().and_then(release_version);
            if current.is_some_and(|current| current >= newest) {
                event.to_version = event.from_version.clone();
                return Ok(false);
            }
            anyhow::ensure!(
                !candidate.unchanged,
                "installer omitted a required newer release"
            );
            validate_candidate(harness, &stage.path, &candidate)?;
            source.compatible(harness, &candidate).await?;
            if event.from_version.as_deref() == Some(&candidate.version) {
                Ok(false)
            } else {
                promote(harness, &stage, &candidate)?;
                Ok::<bool, anyhow::Error>(true)
            }
        })
        .await;
        match result {
            Ok(Ok(updated)) => {
                event.status = if updated { "updated" } else { "current" }.into();
                event.message = if updated {
                    "Updated after compatibility checks"
                } else {
                    "Installed release is current or newer"
                }
                .into();
            }
            Ok(Err(error)) => {
                event.status = "warning".into();
                event.message = format!("Keeping the installed version: {error}");
            }
            Err(_) => {
                event.status = "warning".into();
                event.message = "Update timed out; keeping the installed version".into();
            }
        }
        emit(event.clone());
        report.push(event);
    }
    report
}

/// Kill the whole updater/probe process group on Linux when a deadline expires.
struct Process {
    child: tokio::process::Child,
    pid: Option<u32>,
}
impl Drop for Process {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }
}
fn spawn(command: &mut tokio::process::Command) -> Result<Process> {
    command
        .env("DISABLE_UPDATES", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.as_std_mut().process_group(0);
    let child = command.spawn()?;
    let pid = child.id();
    Ok(Process { child, pid })
}
async fn bounded_output(mut reader: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if kept.len() + n > 65536 {
            anyhow::bail!("process output exceeded 64 KiB");
        }
        kept.extend_from_slice(&chunk[..n]);
    }
    Ok(kept)
}
async fn command_output(
    command: &mut tokio::process::Command,
    budget: Duration,
) -> Result<Vec<u8>> {
    command_output_with_stage_error(command, budget, false).await
}

async fn command_output_with_stage_error(
    command: &mut tokio::process::Command,
    budget: Duration,
    stage_error: bool,
) -> Result<Vec<u8>> {
    let mut process = spawn(command)?;
    drop(process.child.stdin.take());
    let stdout = process.child.stdout.take().unwrap();
    let stderr = process.child.stderr.take().unwrap();
    let output = tokio::time::timeout(budget, async {
        let (out, diagnostic) = tokio::try_join!(bounded_output(stdout), bounded_output(stderr))?;
        let status = process.child.wait().await?;
        process.pid = None;
        if !status.success() && stage_error {
            if let Some(reason) = String::from_utf8_lossy(&diagnostic)
                .lines()
                .find(|line| line.starts_with("Harness staging failed: "))
            {
                let message: String = reason
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(300)
                    .collect();
                anyhow::bail!("{message}");
            }
        }
        anyhow::ensure!(status.success(), "process exited with {status}");
        Ok::<_, anyhow::Error>(out)
    })
    .await
    .context("process timed out")??;
    // Do not signal a reaped PID that the OS could have reused.
    process.pid = None;
    Ok(output)
}
pub struct OfficialSource;
#[async_trait]
impl UpdateSource for OfficialSource {
    fn installed(&self, harness: Harness) -> Option<PathBuf> {
        if std::env::var_os(harness.override_name()).is_some()
            || (harness == Harness::Codex && std::env::var_os("BS_CODEX_CODE_MODE_HOST").is_some())
        {
            return None;
        }
        managed_binary(&managed_root(), harness).or_else(|| {
            if harness == Harness::Claude {
                return crate::agents::claude::host_claude_bin();
            }
            let file = crate::setup::host_home()
                .join(".local/bin")
                .join(harness.name());
            file.is_file().then_some(file)
        })
    }
    async fn version(&self, binary: &Path) -> Result<String> {
        let output = command_output(
            tokio::process::Command::new(binary).arg("--version"),
            Duration::from_secs(10),
        )
        .await?;
        String::from_utf8_lossy(&output)
            .split_whitespace()
            .find(|s| release_version(s).is_some())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("unrecognized installed version"))
    }
    async fn stage(
        &self,
        harness: Harness,
        directory: &Path,
        current_version: Option<&str>,
    ) -> Result<Candidate> {
        let mut command = tokio::process::Command::new("bash");
        command
            .arg("-c")
            .arg(include_str!("../../../scripts/stage-harness.sh"))
            .arg("bondsymphonic-update")
            .arg(harness.name())
            .arg("--stage-dir")
            .arg(directory);
        if let Some(version) = current_version.filter(|v| release_version(v).is_some()) {
            command.arg("--current-version").arg(version);
        }
        let output =
            command_output_with_stage_error(&mut command, Duration::from_secs(90), true).await?;
        String::from_utf8_lossy(&output)
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str(line).ok())
            .ok_or_else(|| anyhow!("installer did not return a release manifest"))
    }
    async fn compatible(&self, harness: Harness, candidate: &Candidate) -> Result<()> {
        let version = self.version(&candidate.binary).await?;
        anyhow::ensure!(
            version == candidate.version,
            "downloaded executable version does not match release metadata"
        );
        if harness == Harness::Claude {
            let help = command_output(
                tokio::process::Command::new(&candidate.binary).arg("--help"),
                Duration::from_secs(10),
            )
            .await?;
            let help = String::from_utf8_lossy(&help);
            for flag in [
                "--input-format",
                "--output-format",
                "--include-partial-messages",
                "--resume",
                "--permission-mode",
                "--mcp-config",
            ] {
                anyhow::ensure!(help.contains(flag), "Claude CLI lacks required {flag}");
            }
            Ok(())
        } else {
            if harness == Harness::Codex {
                let helper = candidate.helper.as_ref().context("Codex helper missing")?;
                let help = command_output(
                    tokio::process::Command::new(helper).arg("--help"),
                    Duration::from_secs(10),
                )
                .await?;
                anyhow::ensure!(
                    String::from_utf8_lossy(&help).contains("--listen"),
                    "Codex helper lacks required transport capability"
                );
            }
            probe_protocol(harness, &candidate.binary).await
        }
    }
}
async fn probe_protocol(harness: Harness, binary: &Path) -> Result<()> {
    let home = binary
        .parent()
        .context("candidate has no parent")?
        .join(".compatibility-home");
    probe_protocol_command(harness, tokio::process::Command::new(binary), &home).await
}

/// The same bounded probe with an injected command for protocol regression tests.
#[doc(hidden)]
pub async fn probe_protocol_command(
    harness: Harness,
    mut command: tokio::process::Command,
    home: &Path,
) -> Result<()> {
    std::fs::create_dir_all(home)?;
    // Codex requires an explicitly supplied CODEX_HOME to exist already.
    std::fs::create_dir_all(home.join("codex"))?;
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("CODEX_HOME", home.join("codex"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_STATE_HOME", home.join("state"))
        .current_dir(home);
    let params = if harness == Harness::Codex {
        command.args(["app-server", "--listen", "stdio://"]);
        json!({"clientInfo":{"name":"bondsymphonic-update","version":env!("CARGO_PKG_VERSION")}})
    } else {
        command
            .arg("acp")
            .env("OPENCODE_DISABLE_AUTOUPDATE", "true")
            .env("OPENCODE_DISABLE_MODELS_FETCH", "true");
        json!({"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":true},"terminal":false},"clientInfo":{"name":"bondsymphonic-update","version":env!("CARGO_PKG_VERSION")}})
    };
    let mut process = spawn(&mut command)?;
    let mut input = process.child.stdin.take().unwrap();
    let mut output = BufReader::new(process.child.stdout.take().unwrap());
    let stderr = process.child.stderr.take().unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        let request = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":params});
        input.write_all(format!("{request}\n").as_bytes()).await?;
        let read = async {
            for _ in 0..32 {
                let mut line = String::new();
                let n = (&mut output).take(65537).read_line(&mut line).await?;
                anyhow::ensure!(
                    n > 0 && n <= 65536,
                    "invalid or oversized initialization reply"
                );
                let reply: Value = serde_json::from_str(&line)?;
                if reply["id"] == 1 {
                    anyhow::ensure!(
                        reply.get("error").is_none() && reply["result"].is_object(),
                        "harness rejected initialization"
                    );
                    if harness == Harness::OpenCode {
                        anyhow::ensure!(
                            reply["result"]["protocolVersion"] == 1
                                && reply["result"]["agentCapabilities"]["loadSession"] == true,
                            "OpenCode lacks required ACP v1/session resume capability"
                        );
                    } else {
                        anyhow::ensure!(
                            reply["result"]["userAgent"].is_string(),
                            "Codex returned unsupported initialization response"
                        );
                    }
                    return Ok::<_, anyhow::Error>(());
                }
            }
            anyhow::bail!("no initialization reply")
        };
        let drain = async {
            bounded_output(stderr).await?;
            std::future::pending::<Result<()>>().await
        };
        tokio::select! {result=read=>result,result=drain=>result}
    })
    .await
    .context("harness compatibility probe timed out")?
}

pub async fn run_cli(data_dir: &Path, watch_stdin: bool) -> Result<()> {
    let _instance = crate::daemon::InstanceLock::acquire(data_dir)?;
    let root = managed_root();
    let _updates = crate::daemon::InstanceLock::acquire(&root)?;
    let updates = update_installed(&root, &OfficialSource, Duration::from_secs(120), |event| {
        println!("{}", serde_json::to_string(&event).unwrap())
    });
    let report = if watch_stdin {
        let (closed, receiver) = tokio::sync::oneshot::channel();
        // A dedicated detached thread avoids Tokio's uncancellable stdin worker,
        // which would otherwise hold runtime shutdown while the IDE keeps stdin open.
        std::thread::spawn(move || {
            use std::io::Read;
            let mut stdin = std::io::stdin();
            let mut bytes = [0u8; 64];
            loop {
                match stdin.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let _ = closed.send(());
        });
        tokio::select! {report=updates=>report,_=receiver=>{anyhow::bail!("Harness update cancelled because the launcher closed stdin");}}
    } else {
        updates.await
    };
    println!("{}", json!({"type":"summary","updates":report}));
    Ok(())
}
