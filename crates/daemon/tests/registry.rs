use bondsymphonic_daemon::workspace::registry::Registry;
use bondsymphonic_daemon::workspace::Workspace;
use bondsymphonic_proto::WorkspaceState;

fn sample(id: &str, name: &str) -> Workspace {
    Workspace {
        id: id.into(),
        name: name.into(),
        repo_path: "/repo".into(),
        base_branch: "main".into(),
        branch: Workspace::branch_for(name),
        worktree_path: format!("/data/worktrees/{id}").into(),
        created_at: "2026-09-08T10:00:00Z".into(),
        allowlist: vec![],
        state: WorkspaceState::Ready,
        agents: vec![],
        runs: vec![],
    }
}

#[test]
fn branch_naming_convention() {
    assert_eq!(Workspace::branch_for("agent-1"), "bs/agent-1/work");
}

#[tokio::test]
async fn registry_roundtrips_through_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    let reg = Registry::load(&path).unwrap();
    assert!(reg.list().is_empty());
    reg.insert(sample("ws_00000001", "a")).await.unwrap();
    reg.insert(sample("ws_00000002", "b")).await.unwrap();
    assert_eq!(reg.list().len(), 2);

    let reg2 = Registry::load(&path).unwrap();
    let names: Vec<String> = reg2.list().into_iter().map(|w| w.name).collect();
    assert_eq!(names, vec!["a", "b"]);
    assert_eq!(reg2.get(&"ws_00000001".into()).unwrap().branch, "bs/a/work");
}

#[tokio::test]
async fn update_and_remove_persist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    let reg = Registry::load(&path).unwrap();
    reg.insert(sample("ws_00000001", "a")).await.unwrap();
    let updated = reg
        .update(&"ws_00000001".into(), |w| {
            w.state = WorkspaceState::Error("boom".into())
        })
        .await
        .unwrap();
    assert_eq!(updated.state, WorkspaceState::Error("boom".into()));
    assert!(reg.remove(&"ws_00000001".into()).await.unwrap().is_some());
    let reg2 = Registry::load(&path).unwrap();
    assert!(reg2.list().is_empty());
    assert!(reg.update(&"ws_nope".into(), |_| {}).await.is_err());
}

#[test]
fn ids_have_prefix_and_hex() {
    let id = bondsymphonic_daemon::ids::new_id("ws_");
    assert!(id.starts_with("ws_"));
    assert_eq!(id.len(), 11);
    assert!(id[3..]
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_ne!(id, bondsymphonic_daemon::ids::new_id("ws_"));
}

/// Workspaces created before the allowlist carried any weight were stored with
/// an empty one, and to the proxy an empty list means "reach nothing" — the
/// agent in such a workspace could not even call the Anthropic API. A
/// version-1 file is migrated once on load: an empty allowlist becomes the
/// defaults, a list that already has entries is left alone, and the file is
/// rewritten as version 2. The migration is keyed on the version rather than
/// on emptiness because `workspace.set_allowlist` can empty a list on purpose,
/// and that has to survive a restart.
#[tokio::test]
async fn a_version_1_registry_gains_the_default_allowlist_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    let defaults = bondsymphonic_daemon::net::allowlist::DEFAULT_ALLOW;

    let mut configured = sample("ws_00000002", "b");
    configured.allowlist = vec!["only.example".into()];
    let legacy = serde_json::json!({
        "version": 1,
        "workspaces": [
            serde_json::to_value(sample("ws_00000001", "a")).unwrap(),
            serde_json::to_value(&configured).unwrap(),
        ],
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

    let reg = Registry::load(&path).unwrap();
    assert_eq!(
        reg.get(&"ws_00000001".into()).unwrap().allowlist,
        defaults.map(String::from).to_vec()
    );
    assert_eq!(
        reg.get(&"ws_00000002".into()).unwrap().allowlist,
        vec!["only.example".to_string()]
    );

    // The migration is written back, so it runs once rather than on every start.
    let on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(on_disk["version"], 2);
    assert_eq!(
        on_disk["workspaces"][0]["allowlist"]
            .as_array()
            .unwrap()
            .len(),
        defaults.len()
    );

    // And from version 2 on, a list emptied on purpose stays empty.
    reg.update(&"ws_00000001".into(), |w| w.allowlist.clear())
        .await
        .unwrap();
    let reg2 = Registry::load(&path).unwrap();
    assert!(reg2
        .get(&"ws_00000001".into())
        .unwrap()
        .allowlist
        .is_empty());
}

/// A registry file the daemon has never written — no `version` key at all —
/// predates every format there has been, so it takes the same migration rather
/// than refusing to load.
#[test]
fn a_registry_file_without_a_version_is_treated_as_the_oldest_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    let legacy = serde_json::json!({
        "workspaces": [serde_json::to_value(sample("ws_00000001", "a")).unwrap()],
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

    let reg = Registry::load(&path).unwrap();
    assert_eq!(
        reg.get(&"ws_00000001".into()).unwrap().allowlist.len(),
        bondsymphonic_daemon::net::allowlist::DEFAULT_ALLOW.len()
    );
}

/// A daemon killed mid-write leaves whatever it was writing behind. If the
/// writer always uses the same sibling name, one such leftover — or one
/// concurrent writer — is enough to make every later save write over the other
/// one's half-finished file, or fail outright and never recover.
///
/// The directory is the version of that leftover no write can clear on its own,
/// which is what makes this a fixed test rather than a race: a writer that
/// reuses one fixed name can never save again, and one that picks a unique name
/// per call is unaffected. The rename itself is still what makes the file whole
/// or untouched and never half of either.
#[tokio::test]
async fn a_leftover_at_the_old_fixed_temporary_name_does_not_wedge_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    let reg = Registry::load(&path).unwrap();
    let squatter = dir.path().join("workspaces.json.tmp");
    std::fs::create_dir(&squatter).unwrap();

    reg.insert(sample("ws_00000001", "a")).await.unwrap();
    assert_eq!(Registry::load(&path).unwrap().list().len(), 1);

    reg.insert(sample("ws_00000002", "b")).await.unwrap();
    let names: Vec<String> = Registry::load(&path)
        .unwrap()
        .list()
        .into_iter()
        .map(|w| w.name)
        .collect();
    assert_eq!(names, vec!["a", "b"]);

    let left: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "workspaces.json" && n != "workspaces.json.tmp")
        .collect();
    assert!(left.is_empty(), "a temporary was left behind: {left:?}");
    assert!(squatter.is_dir(), "and what was in the way is untouched");
}

/// The registry is written from request handlers running on the daemon's
/// runtime, so the `fsync` is moved off the worker rather than left to stall
/// every other connection while a busy disk finishes. It goes to the blocking
/// pool, which every runtime flavour has, and the write is awaited, so a
/// handler that reports success reports a file that really was written.
///
/// The one write with no runtime to hand anything to is the migration in
/// `Registry::load`, called from `main` before the runtime is built. That one
/// writes on the spot, and has to, or a version-1 registry would be migrated
/// again at every start.
/// A plain `#[test]`, because it builds a runtime of each flavour itself and
/// `block_on` cannot be called from inside one.
#[test]
fn the_registry_is_written_from_either_runtime_and_by_the_startup_migration() {
    let dir = tempfile::tempdir().unwrap();

    // No runtime at all: the daemon's own startup path, on a thread that has
    // never heard of tokio.
    let bare = dir.path().join("bare.json");
    let legacy = serde_json::json!({
        "version": 1,
        "workspaces": [serde_json::to_value(sample("ws_00000001", "a")).unwrap()],
    });
    std::fs::write(&bare, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
    assert_eq!(Registry::load(&bare).unwrap().list().len(), 1);
    let on_disk: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&bare).unwrap()).unwrap();
    assert_eq!(on_disk["version"], 2, "the migration was not written");

    for (name, rt) in [
        (
            "multi_thread",
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap(),
        ),
        (
            "current_thread",
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        ),
    ] {
        let path = dir.path().join(format!("{name}.json"));
        rt.block_on(async {
            let reg = Registry::load(&path).unwrap();
            reg.insert(sample("ws_00000001", "a")).await.unwrap();
            reg.update(&"ws_00000001".into(), |w| {
                w.state = WorkspaceState::Error("boom".into())
            })
            .await
            .unwrap();
            reg.remove(&"ws_00000001".into()).await.unwrap();
            reg.insert(sample("ws_00000002", "b")).await.unwrap();
        });
        let names: Vec<String> = Registry::load(&path)
            .unwrap()
            .list()
            .into_iter()
            .map(|w| w.name)
            .collect();
        assert_eq!(names, vec!["b"], "on the {name} runtime");
    }
}

/// A registry write must leave the task that made it on the worker it was
/// running on.
///
/// The `fsync` belongs off the runtime -- that is what
/// [`the_registry_is_written_from_either_runtime_and_from_none`] is about --
/// but *how* it leaves matters as much as that it leaves.
/// `tokio::task::block_in_place` moves it by handing this worker's core to a
/// sibling thread and running the write here; the thread that gave up its core
/// goes back to tokio's blocking pool, and an unused thread there exits ten
/// seconds later. Anything that thread had forked with
/// `prctl(PR_SET_PDEATHSIG)` armed -- which is every workspace sandbox, through
/// `bwrap --die-with-parent` -- is SIGKILLed with it.
///
/// One worker, so there is exactly one thread to watch, and the write is made
/// from a spawned task: a future driven by `block_on` holds no worker core and
/// `block_in_place` on such a thread does nothing at all. The sampling
/// straddles a yield because the hand-off only shows once the task is polled
/// again.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_registry_write_leaves_the_worker_it_ran_on_running() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.json");
    let reg = Registry::load(&path).unwrap();
    reg.insert(sample("ws_00000001", "a")).await.unwrap();

    let seen = tokio::spawn(async move {
        let mut seen = std::collections::HashSet::new();
        for i in 0..20u32 {
            seen.insert(std::thread::current().id());
            reg.update(&"ws_00000001".into(), |w| {
                w.state = WorkspaceState::Error(i.to_string())
            })
            .await
            .unwrap();
            tokio::task::yield_now().await;
            seen.insert(std::thread::current().id());
        }
        seen
    })
    .await
    .unwrap();

    assert_eq!(
        seen.len(),
        1,
        "the registry write moved the task off its worker, leaving a thread to retire: {seen:?}"
    );
}
