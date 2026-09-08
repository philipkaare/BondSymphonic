use bondsymphonic_proto::PrereqStatus;
use std::process::Stdio;
use tokio::process::Command;

pub fn parse_git_version(s: &str) -> Option<(u32, u32)> {
    let v = s.strip_prefix("git version ")?;
    let mut it = v.split('.');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

async fn run(cmd: &str, args: &[&str]) -> Result<(bool, String), String> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Ok((
        out.status.success(),
        if text.is_empty() { err } else { text },
    ))
}

fn status(name: &str, ok: bool, detail: impl Into<String>, fix: &str) -> PrereqStatus {
    PrereqStatus {
        name: name.into(),
        ok,
        detail: detail.into(),
        fix_hint: if ok { None } else { Some(fix.into()) },
    }
}

pub async fn check_binary(bin: &str, args: &[&str], fix: &str) -> PrereqStatus {
    match run(bin, args).await {
        Ok((true, out)) => status(bin, true, out.lines().next().unwrap_or("").to_string(), fix),
        Ok((false, out)) => status(bin, false, format!("{bin} exited with error: {out}"), fix),
        Err(e) => status(bin, false, format!("{bin} not found: {e}"), fix),
    }
}

fn home() -> std::path::PathBuf {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .unwrap_or_default()
}

pub async fn check_all() -> Vec<PrereqStatus> {
    let mut v = Vec::new();

    let git = match run("git", &["--version"]).await {
        Ok((true, out)) => match parse_git_version(&out) {
            Some((maj, min)) if (maj, min) >= (2, 40) => status("git", true, out, ""),
            Some(_) => status(
                "git",
                false,
                format!("{out} is older than 2.40"),
                "sudo apt-get install -y git",
            ),
            None => status("git", false, out, "sudo apt-get install -y git"),
        },
        _ => status("git", false, "git not found", "sudo apt-get install -y git"),
    };
    v.push(git);

    let mut bwrap = check_binary(
        "bwrap",
        &["--version"],
        "sudo apt-get install -y bubblewrap",
    )
    .await;
    bwrap.name = "bwrap".into();
    let bwrap_ok = bwrap.ok;
    v.push(bwrap);

    let userns = if bwrap_ok {
        match run(
            "bwrap",
            &["--ro-bind", "/", "/", "--unshare-all", "--die-with-parent", "true"],
        )
        .await
        {
            Ok((true, _)) => status("userns", true, "unprivileged user namespaces work", ""),
            Ok((false, out)) => status(
                "userns",
                false,
                out,
                "echo kernel.apparmor_restrict_unprivileged_userns=0 | sudo tee /etc/sysctl.d/60-bondsymphonic.conf && sudo sysctl --system",
            ),
            Err(e) => status("userns", false, e, "sudo apt-get install -y bubblewrap"),
        }
    } else {
        status(
            "userns",
            false,
            "bwrap missing",
            "sudo apt-get install -y bubblewrap",
        )
    };
    v.push(userns);

    let claude_path = home().join(".local/bin/claude");
    let claude_bin = if claude_path.exists() {
        claude_path.to_string_lossy().to_string()
    } else {
        "claude".into()
    };
    let mut claude = check_binary(
        &claude_bin,
        &["--version"],
        "curl -fsSL https://claude.ai/install.sh | bash",
    )
    .await;
    claude.name = "claude".into();
    v.push(claude);

    let creds = home().join(".claude/.credentials.json");
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map(|k| !k.is_empty())
        .unwrap_or(false);
    v.push(status(
        "claude_auth",
        creds.exists() || api_key,
        if api_key {
            "ANTHROPIC_API_KEY set"
        } else if creds.exists() {
            "OAuth credentials present"
        } else {
            "no credentials"
        },
        "run `claude` once in the distro and log in, or set ANTHROPIC_API_KEY",
    ));

    let mut gh = check_binary("gh", &["--version"], "sudo apt-get install -y gh").await;
    gh.name = "gh".into();
    let gh_ok = gh.ok;
    v.push(gh);

    let gh_auth = if gh_ok {
        match run("gh", &["auth", "status"]).await {
            Ok((true, out)) => status(
                "gh_auth",
                true,
                out.lines().next().unwrap_or("").to_string(),
                "",
            ),
            Ok((false, out)) => status("gh_auth", false, out, "gh auth login"),
            Err(e) => status("gh_auth", false, e, "gh auth login"),
        }
    } else {
        status(
            "gh_auth",
            false,
            "gh missing",
            "sudo apt-get install -y gh && gh auth login",
        )
    };
    v.push(gh_auth);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_git_version() {
        assert_eq!(
            parse_git_version("git version 2.52.0.windows.1"),
            Some((2, 52))
        );
        assert_eq!(parse_git_version("git version 2.43.0"), Some((2, 43)));
        assert_eq!(parse_git_version("nonsense"), None);
    }
    #[tokio::test]
    async fn missing_binary_reports_fix_hint() {
        let s = check_binary(
            "definitely-not-a-real-binary-xyz",
            &["--version"],
            "install it",
        )
        .await;
        assert!(!s.ok);
        assert_eq!(s.fix_hint.as_deref(), Some("install it"));
    }
    #[tokio::test]
    async fn check_all_has_stable_order() {
        let names: Vec<String> = check_all().await.into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            [
                "git",
                "bwrap",
                "userns",
                "claude",
                "claude_auth",
                "gh",
                "gh_auth"
            ]
        );
    }
}
