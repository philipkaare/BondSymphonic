//! Secrets the user hands the sandboxes as environment variables: some for
//! every repository, some for one.
//!
//! The values live in one private file under the daemon's data directory
//! (`secrets/env.json`, `0600`), which no sandbox can see: the data directory
//! is masked in every one. They reach a sandbox only as the environment of the
//! processes started in it -- the agent, its terminals, runs, local MCP
//! servers -- and travel there over the sandbox's exec socket with each start,
//! never on a command line: bwrap's own arguments are readable by every
//! process on the machine. So a change applies to the next process started,
//! and a running agent sees it once it is restarted.
//!
//! A repository's secret of the same name wins over the every-repository one.
//! The environment the sandbox sets itself (`PATH`, `HOME`, the proxy, the git
//! routing, `BS_*`) is never overridden, and names that would are refused when
//! they are set.

use crate::sandbox::{SandboxChild, SandboxCommand, SandboxHandle};
use async_trait::async_trait;
use bondsymphonic_proto::{RpcError, SecretInfo, SecretListResult};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Longest name accepted.
const MAX_NAME: usize = 128;
/// Largest value accepted: room for a certificate or a service-account key,
/// not for a file that belongs on disk.
const MAX_VALUE: usize = 64 * 1024;

/// Names the sandbox sets itself, or that change what its git or its loader
/// does. Matched exactly; see also [`RESERVED_PREFIXES`].
const RESERVED: [&str; 20] = [
    "HOME",
    "USER",
    "LOGNAME",
    "PATH",
    "SHELL",
    "PWD",
    "TERM",
    "LANG",
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
];
/// The daemon's own variables, and git's: the shim routes the workspace's
/// objects with `GIT_*`, and a secret that set one would send them elsewhere.
const RESERVED_PREFIXES: [&str; 2] = ["BS_", "GIT_"];

#[derive(Default, Serialize, Deserialize)]
struct SecretFile {
    /// For every repository: name -> value.
    #[serde(default)]
    global: BTreeMap<String, String>,
    /// Per repository, keyed by its path as the daemon sees it, made canonical.
    #[serde(default)]
    repos: BTreeMap<String, BTreeMap<String, String>>,
}

pub struct EnvSecrets {
    path: PathBuf,
    file: Mutex<SecretFile>,
}

impl EnvSecrets {
    /// The store in `root`'s `secrets/env.json`. A file that cannot be read
    /// is logged and treated as empty, and is only replaced when the user next
    /// sets or removes a secret.
    pub fn load(root: &Path) -> EnvSecrets {
        let path = root.join("secrets").join("env.json");
        let file = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), "environment secrets unreadable: {e}");
                SecretFile::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SecretFile::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), "environment secrets unreadable: {e}");
                SecretFile::default()
            }
        };
        EnvSecrets {
            path,
            file: Mutex::new(file),
        }
    }

    /// Names and scopes, never values.
    pub fn list(&self) -> SecretListResult {
        let file = self.file.lock();
        let mut secrets: Vec<SecretInfo> = file
            .global
            .keys()
            .map(|name| SecretInfo {
                name: name.clone(),
                repo: None,
            })
            .collect();
        for (repo, names) in &file.repos {
            secrets.extend(names.keys().map(|name| SecretInfo {
                name: name.clone(),
                repo: Some(repo.clone()),
            }));
        }
        SecretListResult { secrets }
    }

    pub fn set(&self, name: &str, value: &str, repo: Option<&str>) -> Result<(), RpcError> {
        check_name(name)?;
        if value.len() > MAX_VALUE {
            return Err(RpcError::invalid_params(format!(
                "the value of {name} is longer than {} KiB",
                MAX_VALUE / 1024
            )));
        }
        if value.contains('\0') {
            return Err(RpcError::invalid_params(format!(
                "the value of {name} contains a NUL byte, which no environment can carry"
            )));
        }
        let key = repo.map(repo_key).transpose()?;
        let mut file = self.file.lock();
        let mut next = SecretFile {
            global: file.global.clone(),
            repos: file.repos.clone(),
        };
        match &key {
            None => next.global.insert(name.to_owned(), value.to_owned()),
            Some(repo) => next
                .repos
                .entry(repo.clone())
                .or_default()
                .insert(name.to_owned(), value.to_owned()),
        };
        self.save(&next)?;
        *file = next;
        Ok(())
    }

    /// Removes one. Removing one that is not there is not an error.
    pub fn remove(&self, name: &str, repo: Option<&str>) -> Result<(), RpcError> {
        let key = repo.map(repo_key).transpose()?;
        let mut file = self.file.lock();
        let mut next = SecretFile {
            global: file.global.clone(),
            repos: file.repos.clone(),
        };
        match &key {
            None => {
                next.global.remove(name);
            }
            Some(repo) => {
                if let Some(names) = next.repos.get_mut(repo) {
                    names.remove(name);
                    if names.is_empty() {
                        next.repos.remove(repo);
                    }
                }
            }
        }
        self.save(&next)?;
        *file = next;
        Ok(())
    }

    /// The environment a sandbox of `repo` gets: every-repository secrets,
    /// then the repository's own over them.
    pub fn env_for(&self, repo: &Path) -> Vec<(String, String)> {
        let key = crate::git::repo::canonical_ish(repo)
            .to_string_lossy()
            .into_owned();
        let file = self.file.lock();
        let mut env = file.global.clone();
        if let Some(own) = file.repos.get(&key) {
            env.extend(own.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        // A file edited by hand is not trusted to have kept to the rules.
        env.into_iter()
            .filter(|(k, _)| check_name(k).is_ok())
            .collect()
    }

    fn save(&self, file: &SecretFile) -> Result<(), RpcError> {
        let bytes =
            serde_json::to_vec_pretty(file).map_err(|e| RpcError::internal(e.to_string()))?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| RpcError::io(&e))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        crate::mcp::servers::secrets::write_private(&self.path, &bytes)
            .map_err(|e| RpcError::io(&e))
    }
}

/// Whether `name` may be a secret's name: an environment variable a shell can
/// name, that the sandbox does not set itself.
pub fn check_name(name: &str) -> Result<(), RpcError> {
    let mut chars = name.chars();
    let well_formed = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !well_formed || name.len() > MAX_NAME {
        return Err(RpcError::invalid_params(format!(
            "{name:?} is not a usable environment variable name: letters, digits and _, \
             not starting with a digit, at most {MAX_NAME} characters"
        )));
    }
    if RESERVED.contains(&name) || RESERVED_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return Err(RpcError::invalid_params(format!(
            "{name} is set by the sandbox itself and cannot be a secret"
        )));
    }
    Ok(())
}

/// A repository path as the store keys it.
fn repo_key(repo: &str) -> Result<String, RpcError> {
    let path = Path::new(repo.trim());
    if !path.is_absolute() {
        return Err(RpcError::invalid_params(format!(
            "{repo:?} is not an absolute path to a repository"
        )));
    }
    Ok(crate::git::repo::canonical_ish(path)
        .to_string_lossy()
        .into_owned())
}

/// A sandbox handle whose every process also gets the repository's secrets.
///
/// Wraps the handle the backend returned, so every way into the sandbox --
/// agents, terminals, runs, MCP servers -- gets them, read from the store at
/// each start. The command's own environment still wins.
pub struct WithSecrets {
    inner: Arc<dyn SandboxHandle>,
    secrets: Arc<EnvSecrets>,
    repo: PathBuf,
}

pub fn with_secrets(
    inner: Arc<dyn SandboxHandle>,
    secrets: Arc<EnvSecrets>,
    repo: &Path,
) -> Arc<dyn SandboxHandle> {
    Arc::new(WithSecrets {
        inner,
        secrets,
        repo: repo.to_path_buf(),
    })
}

#[async_trait]
impl SandboxHandle for WithSecrets {
    async fn spawn(&self, mut cmd: SandboxCommand) -> Result<SandboxChild, RpcError> {
        let mut env = self.secrets.env_for(&self.repo);
        env.append(&mut cmd.env);
        cmd.env = env;
        self.inner.spawn(cmd).await
    }

    async fn shutdown(&self) -> Result<(), RpcError> {
        self.inner.shutdown().await
    }

    fn helper_exe(&self) -> PathBuf {
        self.inner.helper_exe()
    }

    fn died(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.inner.died()
    }

    fn host_pid(&self) -> Option<u32> {
        self.inner.host_pid()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_shell_names_the_sandbox_does_not_set() {
        for ok in ["API_TOKEN", "_x", "a1", "NPM_TOKEN", "ANTHROPIC_API_KEY"] {
            assert!(check_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "1A",
            "A-B",
            "A B",
            "PATH",
            "HOME",
            "https_proxy",
            "BS_WORKSPACE",
            "GIT_DIR",
            "LD_PRELOAD",
            "Å",
        ] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_name(&"A".repeat(MAX_NAME + 1)).is_err());
    }

    #[test]
    fn a_repositorys_secret_wins_and_nothing_is_listed_with_its_value() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let s = EnvSecrets::load(dir.path());
        s.set("TOKEN", "global", None).unwrap();
        s.set("SHARED", "everywhere", None).unwrap();
        s.set("TOKEN", "mine", Some(repo.to_str().unwrap()))
            .unwrap();
        assert!(s.set("PATH", "/evil", None).is_err());
        assert!(s.set("X", "a\0b", None).is_err());
        assert!(s.set("X", "v", Some("relative/path")).is_err());

        let env: BTreeMap<_, _> = s.env_for(&repo).into_iter().collect();
        assert_eq!(env["TOKEN"], "mine");
        assert_eq!(env["SHARED"], "everywhere");
        let env: BTreeMap<_, _> = s.env_for(&other).into_iter().collect();
        assert_eq!(env["TOKEN"], "global");

        let listed = serde_json::to_string(&s.list()).unwrap();
        assert!(
            !listed.contains("mine") && !listed.contains("global\""),
            "{listed}"
        );
        assert_eq!(s.list().secrets.len(), 3);

        // Kept across a restart, in a file only its owner can read.
        let again = EnvSecrets::load(dir.path());
        assert_eq!(again.list(), s.list());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("secrets/env.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }

        again.remove("TOKEN", Some(repo.to_str().unwrap())).unwrap();
        let env: BTreeMap<_, _> = again.env_for(&repo).into_iter().collect();
        assert_eq!(env["TOKEN"], "global");
        again.remove("TOKEN", None).unwrap();
        again.remove("NEVER_SET", None).unwrap();
        assert_eq!(again.list().secrets.len(), 1);
    }
}
