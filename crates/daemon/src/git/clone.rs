//! `repo.github_list` and `repo.clone`: the user's GitHub repositories, and a
//! fresh checkout of one to start a workspace on.
//!
//! Both run `gh` on the host as the user -- the login the Setup page made --
//! never inside a sandbox, which holds no credentials. The repository is
//! named by the client and checked here before it reaches `gh`, and the clone
//! goes into a new directory of its own under a folder the user chose.

use crate::git::gh;
use bondsymphonic_proto::{GithubRepo, GithubRepoListResult, RepoCloneResult, RpcError};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a clone may take: a large repository over a slow line.
pub const CLONE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How many repositories the list asks GitHub for.
const LIST_LIMIT: &str = "300";

/// `owner/name` for what the user gave: `owner/name` itself, or a GitHub URL
/// for it (`https://github.com/owner/name`, with or without `.git`, or
/// `git@github.com:owner/name.git`). Anything else -- another host, a path, a
/// name that could be read as an option -- is refused.
pub fn normalize_repo(input: &str) -> Result<(String, String), RpcError> {
    let raw = input.trim();
    let rest = raw
        .strip_prefix("https://github.com/")
        .or_else(|| raw.strip_prefix("http://github.com/"))
        .or_else(|| raw.strip_prefix("git@github.com:"))
        .or_else(|| raw.strip_prefix("github.com/"))
        .unwrap_or(raw);
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let refused = || {
        RpcError::invalid_params(format!(
            "{raw:?} is not a GitHub repository: give it as owner/name or its github.com URL"
        ))
    };
    let (owner, name) = rest.split_once('/').ok_or_else(refused)?;
    let owner_ok = !owner.is_empty()
        && owner.len() <= 39
        && !owner.starts_with('-')
        && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let name_ok = !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !owner_ok || !name_ok {
        return Err(refused());
    }
    Ok((format!("{owner}/{name}"), name.to_owned()))
}

/// The user's own repositories, most recently pushed first, as `gh` lists
/// them. Repositories of an organisation can still be cloned by name.
pub async fn list() -> Result<GithubRepoListResult, RpcError> {
    let args: Vec<String> = [
        "repo",
        "list",
        "--limit",
        LIST_LIMIT,
        "--json",
        "nameWithOwner,description,isPrivate",
    ]
    .map(str::to_owned)
    .to_vec();
    let out = gh::run_gh(&crate::setup::host_home(), &args, "gh repo list").await?;
    parse_list(&out.stdout)
}

pub fn parse_list(json: &str) -> Result<GithubRepoListResult, RpcError> {
    let values: Vec<Value> = serde_json::from_str(json).map_err(|e| {
        RpcError::internal(format!("gh repo list answered something unreadable: {e}"))
    })?;
    let repos = values
        .into_iter()
        .filter_map(|v| {
            Some(GithubRepo {
                name_with_owner: v.get("nameWithOwner")?.as_str()?.to_owned(),
                description: v
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|d| !d.is_empty())
                    .map(str::to_owned),
                private: v.get("isPrivate").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect();
    Ok(GithubRepoListResult { repos })
}

/// Where a clone of `name` under `parent_dir` goes, refusing a folder that is
/// not there and a checkout that would land on something already there.
pub fn clone_target(parent_dir: &str, name: &str) -> Result<PathBuf, RpcError> {
    let parent = Path::new(parent_dir.trim());
    if !parent.is_absolute() {
        return Err(RpcError::invalid_params(format!(
            "{} is not an absolute folder",
            parent.display()
        )));
    }
    if !parent.is_dir() {
        return Err(RpcError::invalid_params(format!(
            "{} is not a folder that exists",
            parent.display()
        )));
    }
    let target = parent.join(name);
    if target.exists() {
        return Err(RpcError::invalid_params(format!(
            "{} already exists: choose another folder, or open that checkout instead",
            target.display()
        )));
    }
    Ok(target)
}

/// Clones `repo` into a new directory under `parent_dir` and answers with its
/// path, which the New Agent dialog then creates the workspace on.
pub async fn clone(repo: &str, parent_dir: &str) -> Result<RepoCloneResult, RpcError> {
    let (slug, name) = normalize_repo(repo)?;
    let target = clone_target(parent_dir, &name)?;
    let args = vec![
        "repo".to_owned(),
        "clone".to_owned(),
        slug,
        crate::git::path_arg(&target),
    ];
    gh::run_gh_within(
        target.parent().unwrap_or(Path::new("/")),
        &args,
        "gh repo clone",
        CLONE_TIMEOUT,
    )
    .await?;
    Ok(RepoCloneResult {
        path: crate::git::path_arg(&target),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repository_is_taken_by_name_or_by_its_github_url() {
        for input in [
            "octo/hello-world",
            " octo/hello-world ",
            "https://github.com/octo/hello-world",
            "https://github.com/octo/hello-world.git",
            "https://github.com/octo/hello-world/",
            "git@github.com:octo/hello-world.git",
            "github.com/octo/hello-world",
        ] {
            assert_eq!(
                normalize_repo(input).unwrap(),
                ("octo/hello-world".to_owned(), "hello-world".to_owned()),
                "{input}"
            );
        }
        for bad in [
            "hello-world",
            "https://gitlab.com/octo/x",
            "octo/../etc",
            "octo/..",
            "--upload-pack=x/y",
            "octo/-x",
            "octo/x/y",
            "octo/x y",
            "",
        ] {
            assert!(normalize_repo(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_clone_goes_into_a_new_folder_under_one_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().to_string_lossy().into_owned();
        assert_eq!(clone_target(&parent, "x").unwrap(), dir.path().join("x"));
        std::fs::create_dir(dir.path().join("x")).unwrap();
        assert!(clone_target(&parent, "x").is_err());
        assert!(clone_target("relative/dir", "x").is_err());
        assert!(clone_target(&format!("{parent}/missing"), "x").is_err());
    }

    #[test]
    fn the_list_reads_what_gh_answers() {
        let list = parse_list(
            r#"[{"nameWithOwner":"octo/a","description":"","isPrivate":true},
                {"nameWithOwner":"octo/b","description":"B","isPrivate":false},
                {"description":"no name"}]"#,
        )
        .unwrap();
        assert_eq!(list.repos.len(), 2);
        assert_eq!(list.repos[0].name_with_owner, "octo/a");
        assert!(list.repos[0].private && list.repos[0].description.is_none());
        assert_eq!(list.repos[1].description.as_deref(), Some("B"));
        assert!(parse_list("not json").is_err());
    }
}
