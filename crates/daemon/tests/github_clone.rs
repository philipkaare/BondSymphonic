//! `repo.github_list` and `repo.clone` end to end, against a stand-in for
//! `gh` (`BS_GH_BIN`) that records what it was asked and makes the checkout
//! with `git init`, so nothing reaches GitHub.

mod common;

use bondsymphonic_proto::*;
use common::{start_daemon, Client};

/// A `gh` that answers `repo list` with two repositories and `repo clone
/// <slug> <dest>` by making `<dest>` a repository, logging every call.
const STUB: &str = r#"
import json, os, subprocess, sys
args = sys.argv[1:]
with open(os.environ["GH_STUB_LOG"], "a") as log:
    log.write(" ".join(args) + "\n")
if args[:2] == ["repo", "list"]:
    print(json.dumps([
        {"nameWithOwner": "octo/one", "description": "First", "isPrivate": False},
        {"nameWithOwner": "octo/two", "description": "", "isPrivate": True},
    ]))
elif args[:2] == ["repo", "clone"]:
    subprocess.run(["git", "init", "-q", args[3]], check=True)
else:
    sys.exit(2)
"#;

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

#[tokio::test]
async fn repositories_are_listed_and_cloned_through_gh() {
    let Some(py) = python() else {
        eprintln!("SKIP: no python interpreter");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let stub = dir.path().join("gh_stub.py");
    std::fs::write(&stub, STUB).unwrap();
    let log = dir.path().join("gh.log");
    std::env::set_var("BS_GH_BIN", format!("{py} \"{}\"", stub.display()));
    std::env::set_var("GH_STUB_LOG", &log);
    let (port, token, _d, cancel) = start_daemon(&dir.path().join("data")).await;
    let mut c = Client::connect(port, &token).await;

    let listed: GithubRepoListResult =
        serde_json::from_value(c.call(Request::RepoGithubList {}).await.unwrap()).unwrap();
    let names: Vec<&str> = listed
        .repos
        .iter()
        .map(|r| r.name_with_owner.as_str())
        .collect();
    assert_eq!(names, ["octo/one", "octo/two"]);
    assert!(listed.repos[1].private);

    let parent = dir.path().join("git");
    std::fs::create_dir(&parent).unwrap();
    let cloned: RepoCloneResult = serde_json::from_value(
        c.call(Request::RepoClone(RepoCloneParams {
            repo: "https://github.com/octo/one.git".into(),
            parent_dir: parent.to_string_lossy().into_owned(),
        }))
        .await
        .unwrap(),
    )
    .unwrap();
    let target = parent.join("one");
    assert_eq!(std::path::Path::new(&cloned.path), target);
    assert!(target.join(".git").is_dir(), "the checkout was not made");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        calls
            .lines()
            .any(|l| l == format!("repo clone octo/one {}", target.display())),
        "{calls}"
    );

    // A second clone of the same name, and a name that is not a repository,
    // are refused before `gh` is run at all.
    let before = std::fs::read_to_string(&log).unwrap().lines().count();
    for repo in ["octo/one", "../../etc"] {
        let err = c
            .call(Request::RepoClone(RepoCloneParams {
                repo: repo.into(),
                parent_dir: parent.to_string_lossy().into_owned(),
            }))
            .await
            .unwrap_err();
        assert_eq!(
            err.code,
            ErrorCode::InvalidParams,
            "{repo}: {}",
            err.message
        );
    }
    assert_eq!(
        std::fs::read_to_string(&log).unwrap().lines().count(),
        before
    );

    std::env::remove_var("BS_GH_BIN");
    cancel.cancel();
}
