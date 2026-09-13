//! `state.json`: what the IDE remembers between runs, and how a restored file
//! is reconciled against the workspaces the daemon still has.
//!
//! Every test here writes into its own directory under the system temp dir.
//! The two that exercise the settings and state *paths* set `BS_SETTINGS_PATH`,
//! `BS_STATE_PATH` and `BS_LEGACY_SETTINGS_PATH` at temp locations, so nothing
//! in this file can read or write the developer's real `%APPDATA%` files.

use bondsymphonic_ide::model::app_state::{TabStatus, Workspaces};
use bondsymphonic_ide::model::persistence::{
    load, port_key, save, temp_path, PersistedGroup, StateFile, StateStore, MAX_RECENT_REPOS,
    STATE_VERSION,
};
use bondsymphonic_proto::{WorkspaceId, WorkspaceInfo, WorkspaceState};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// The environment is process-wide, so the two path tests take turns rather
/// than racing each other's overrides.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// A directory of this test binary's own, named after the test that asked for
/// it, so a failure leaves evidence that cannot be confused with another run's.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bs-persist-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn info(id: &str, name: &str) -> WorkspaceInfo {
    WorkspaceInfo {
        id: WorkspaceId(id.to_owned()),
        name: name.to_owned(),
        repo_path: "/repo".to_owned(),
        base_branch: "main".to_owned(),
        branch: format!("bs/{name}/work"),
        worktree_path: format!("/wt/{name}"),
        created_at: "2026-09-10T10:00:00Z".to_owned(),
        allowlist: Vec::new(),
        state: WorkspaceState::Ready,
        agents: Vec::new(),
        agent_records: Vec::new(),
        runs: Vec::new(),
    }
}

fn group(name: &str, ids: &[&str]) -> PersistedGroup {
    PersistedGroup {
        name: name.to_owned(),
        workspace_ids: ids.iter().map(|s| (*s).to_owned()).collect(),
        tabs: Vec::new(),
    }
}

/// Every field survives the trip through the file, and a fresh file carries
/// the version the loader will recognise next time.
#[test]
fn a_state_file_round_trips_every_field() {
    let dir = temp_dir("roundtrip");
    let path = dir.join("state.json");

    let mut state = StateFile::default();
    assert_eq!(state.version, STATE_VERSION);
    state.groups = vec![group("Backend", &["ws_1", "ws_2"]), group("Empty", &[])];
    state.active_workspace = Some("ws_2".to_owned());
    state.open_editors = BTreeMap::from([(
        "ws_1".to_owned(),
        vec!["src/main.rs".to_owned(), "README.md".to_owned()],
    )]);
    state.active_editor = BTreeMap::from([("ws_1".to_owned(), "src/main.rs".to_owned())]);
    state.splitter_sizes = vec![300, 700];
    state.swapped = true;
    state.recent_repos = vec!["C:/git/one".to_owned(), "C:/git/two".to_owned()];
    state.window_state_b64 = "d2luZG93".to_owned();
    state.geometry_b64 = "Z2VvbQ==".to_owned();
    state.port_overrides = BTreeMap::from([(port_key("ws_1", "web"), 8123u16)]);

    save(&path, &state).expect("state.json written");
    assert_eq!(load(&path), state);

    // Nothing is left behind by the atomic write.
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .expect("temp dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "state.json")
        .collect();
    assert!(leftovers.is_empty(), "unexpected files: {leftovers:?}");

    // A second save replaces the file rather than appending to it.
    let mut second = state.clone();
    second.swapped = false;
    save(&path, &second).expect("state.json rewritten");
    assert_eq!(load(&path), second);
}

/// A file that is not there yet is a first run, not a failure.
#[test]
fn a_missing_state_file_reads_as_the_default() {
    let dir = temp_dir("missing");
    let loaded = load(&dir.join("state.json"));
    assert_eq!(loaded, StateFile::default());
    assert!(loaded.groups.is_empty());
    assert_eq!(loaded.version, STATE_VERSION);
    assert!(!dir.join("state.json").exists(), "load must not create it");
}

/// A half-written or hand-edited file must not cost the user their groups
/// silently: it is moved aside so it can be inspected, and the IDE starts on
/// the defaults.
#[test]
fn a_state_file_that_is_not_utf8_is_moved_aside_too() {
    let dir = temp_dir("corrupt-utf8");
    let path = dir.join("state.json");
    std::fs::write(&path, &[b'{', 0xff, 0xfe, 0x00, b'}'][..]).expect("corrupt file");
    let state = load(&path);
    assert_eq!(state, StateFile::default());
    assert!(
        dir.join("state.json.corrupt").exists(),
        "non-UTF-8 bytes are kept aside"
    );
    assert!(
        !path.exists(),
        "the corrupt file is not left in place to be overwritten"
    );
}

#[test]
fn a_corrupt_state_file_is_moved_aside_and_reads_as_the_default() {
    let dir = temp_dir("corrupt");
    let path = dir.join("state.json");
    std::fs::write(&path, "{\"groups\": [ not json").expect("corrupt file");

    assert_eq!(load(&path), StateFile::default());

    let aside = dir.join("state.json.corrupt");
    assert!(aside.exists(), "the corrupt file is kept as .corrupt");
    assert_eq!(
        std::fs::read_to_string(&aside).expect("aside"),
        "{\"groups\": [ not json"
    );
    assert!(!path.exists(), "the corrupt file is not left in place");

    // A second corrupt file replaces the first one aside rather than failing.
    std::fs::write(&path, "also not json").expect("corrupt file");
    assert_eq!(load(&path), StateFile::default());
    assert_eq!(
        std::fs::read_to_string(&aside).expect("aside"),
        "also not json"
    );
}

/// Most recent first, no duplicates, and never more than ten.
#[test]
fn note_recent_dedups_and_caps_the_list() {
    let mut state = StateFile::default();
    state.note_recent("C:/git/one");
    state.note_recent("C:/git/two");
    state.note_recent("C:/git/one");
    assert_eq!(state.recent_repos, ["C:/git/one", "C:/git/two"]);

    for i in 0..20 {
        state.note_recent(&format!("C:/git/r{i}"));
    }
    assert_eq!(state.recent_repos.len(), MAX_RECENT_REPOS);
    assert_eq!(state.recent_repos[0], "C:/git/r19");
    assert_eq!(state.recent_repos[MAX_RECENT_REPOS - 1], "C:/git/r10");

    // An empty path is not a repository anyone can reopen.
    let before = state.recent_repos.clone();
    state.note_recent("");
    assert_eq!(state.recent_repos, before);
}

/// Workspaces the daemon no longer has leave nothing behind: no group entry,
/// no editor list, no port override, and not the active selection either.
#[test]
fn prune_drops_everything_belonging_to_a_workspace_the_daemon_lost() {
    let mut state = StateFile {
        groups: vec![group("Backend", &["ws_1", "ws_gone"]), group("Empty", &[])],
        active_workspace: Some("ws_gone".to_owned()),
        open_editors: BTreeMap::from([
            ("ws_1".to_owned(), vec!["a.rs".to_owned()]),
            ("ws_gone".to_owned(), vec!["b.rs".to_owned()]),
        ]),
        active_editor: BTreeMap::from([
            ("ws_1".to_owned(), "a.rs".to_owned()),
            ("ws_gone".to_owned(), "b.rs".to_owned()),
        ]),
        port_overrides: BTreeMap::from([
            (port_key("ws_1", "web"), 8080u16),
            (port_key("ws_gone", "web"), 9090u16),
        ]),
        ..StateFile::default()
    };

    state.prune(&["ws_1".to_owned()]);

    assert_eq!(state.groups[0].workspace_ids, ["ws_1"]);
    // An empty group is the user's, not a leftover: pruning keeps it.
    assert_eq!(state.groups.len(), 2);
    assert_eq!(state.groups[1].name, "Empty");
    assert_eq!(state.active_workspace, None);
    assert_eq!(state.open_editors.keys().collect::<Vec<_>>(), ["ws_1"]);
    assert_eq!(state.active_editor.keys().collect::<Vec<_>>(), ["ws_1"]);
    assert_eq!(
        state.port_overrides.keys().collect::<Vec<_>>(),
        [&port_key("ws_1", "web")]
    );
}

/// A port override is remembered per workspace *and* configuration, and set
/// back to none by clearing it.
#[test]
fn port_overrides_are_keyed_by_workspace_and_configuration() {
    let mut state = StateFile::default();
    assert_eq!(state.port_override("ws_1", "web"), None);

    state.set_port_override("ws_1", "web", Some(8123));
    state.set_port_override("ws_1", "api", Some(9000));
    state.set_port_override("ws_2", "web", Some(7000));
    assert_eq!(state.port_override("ws_1", "web"), Some(8123));
    assert_eq!(state.port_override("ws_1", "api"), Some(9000));
    assert_eq!(state.port_override("ws_2", "web"), Some(7000));

    state.set_port_override("ws_1", "web", None);
    assert_eq!(state.port_override("ws_1", "web"), None);
    assert_eq!(state.port_override("ws_1", "api"), Some(9000));
}

/// The persisted order is the user's order, the workspaces the daemon still
/// has keep their places, an empty group is kept, and a workspace the daemon
/// has that no group claims lands in "Unsorted".
#[test]
fn from_persisted_keeps_the_order_and_files_the_rest_into_unsorted() {
    let groups = vec![
        group("Backend", &["ws_2", "ws_gone", "ws_1"]),
        group("Empty", &[]),
    ];
    let list = vec![
        info("ws_1", "alpha"),
        info("ws_2", "beta"),
        info("ws_3", "new"),
    ];

    let ws = Workspaces::from_persisted(&groups, &list, Some("ws_1"));

    assert_eq!(
        ws.groups
            .iter()
            .map(|g| g.name.as_str())
            .collect::<Vec<_>>(),
        ["Backend", "Empty", "Unsorted"]
    );
    // Persisted order, minus the workspace the daemon lost.
    assert_eq!(
        ws.groups[0]
            .tabs
            .iter()
            .map(|t| t.workspace_id.as_str())
            .collect::<Vec<_>>(),
        ["ws_2", "ws_1"]
    );
    assert!(ws.groups[1].tabs.is_empty(), "an empty group is kept");
    assert_eq!(
        ws.groups[2]
            .tabs
            .iter()
            .map(|t| t.workspace_id.as_str())
            .collect::<Vec<_>>(),
        ["ws_3"]
    );

    // Tabs are built from the daemon's own description, like `reconcile` does.
    let restored = ws.active().expect("the persisted active tab");
    assert_eq!(restored.workspace_id.as_str(), "ws_1");
    assert_eq!(restored.name, "alpha");
    assert_eq!(restored.branch, "bs/alpha/work");
    assert_eq!(restored.worktree_path, "/wt/alpha");
    assert_eq!(restored.status, TabStatus::Idle);

    // And the groups come back out in the shape they went in, minus the id
    // the daemon no longer has.
    assert_eq!(
        ws.persisted_groups(),
        vec![
            group("Backend", &["ws_2", "ws_1"]),
            group("Empty", &[]),
            group("Unsorted", &["ws_3"]),
        ]
    );
}

/// With no persisted active workspace, or one the daemon has lost, the first
/// tab is selected rather than nothing.
#[test]
fn from_persisted_falls_back_to_the_first_tab() {
    let groups = vec![group("Empty", &[]), group("Backend", &["ws_1"])];
    let list = vec![info("ws_1", "alpha")];

    for active in [None, Some("ws_gone")] {
        let ws = Workspaces::from_persisted(&groups, &list, active);
        assert_eq!(ws.active_group, 1);
        assert_eq!(ws.active_tab, 0);
        assert_eq!(
            ws.active().map(|t| t.workspace_id.as_str()),
            Some("ws_1"),
            "active={active:?}"
        );
    }
}

/// Nothing persisted and nothing running is still a usable model: one group,
/// no tabs.
#[test]
fn from_persisted_with_nothing_to_restore_is_the_default_model() {
    let ws = Workspaces::from_persisted(&[], &[], None);
    assert_eq!(ws.groups.len(), 1);
    assert!(ws.groups[0].tabs.is_empty());
    assert_eq!(ws.active(), None);

    // A daemon list with no persisted groups at all files everything under
    // "Unsorted", which is what a first run after the feature landed looks like.
    let ws = Workspaces::from_persisted(&[], &[info("ws_1", "alpha")], None);
    assert_eq!(ws.groups.len(), 1);
    assert_eq!(ws.groups[0].name, "Unsorted");
    assert_eq!(ws.groups[0].tabs.len(), 1);
}

/// A workspace named by two groups is placed once, in the first group that
/// claims it, so a hand-edited file cannot produce two tabs for one workspace.
#[test]
fn from_persisted_places_a_duplicated_id_once() {
    let groups = vec![group("A", &["ws_1"]), group("B", &["ws_1"])];
    let ws = Workspaces::from_persisted(&groups, &[info("ws_1", "alpha")], None);
    assert_eq!(ws.groups[0].tabs.len(), 1);
    assert!(ws.groups[1].tabs.is_empty());
}

/// The debounced writer coalesces a burst of changes into one write: every
/// change hands out a token, and only the newest token's timer writes.
#[test]
fn the_store_coalesces_a_burst_into_a_single_write() {
    let dir = temp_dir("debounce");
    let path = dir.join("state.json");
    let store = StateStore::new(Some(path.clone()));

    let first = store.update(|s| s.swapped = true);
    let second = store.update(|s| s.geometry_b64 = "Z2VvbQ==".to_owned());
    assert_ne!(first, second);

    // The first timer wakes after the second change was made: it must not
    // write, or the debounce would be a write per change after all.
    assert!(!store.flush_if_current(first));
    assert!(!path.exists(), "nothing written yet");

    assert!(store.flush_if_current(second), "the last timer writes");
    let written = load(&path);
    assert!(written.swapped);
    assert_eq!(written.geometry_b64, "Z2VvbQ==");

    // Clean now: the same timer firing twice does not write again.
    assert!(!store.flush_if_current(second));
}

/// `flushState()` on exit writes what is pending, and does nothing when
/// nothing is.
#[test]
fn flush_writes_pending_changes_and_is_a_noop_when_clean() {
    let dir = temp_dir("flush");
    let path = dir.join("state.json");
    let store = StateStore::new(Some(path.clone()));

    assert!(!store.flush(), "a store nobody changed writes nothing");
    assert!(!path.exists());

    store.update(|s| s.note_recent("C:/git/one"));
    assert!(store.flush());
    assert_eq!(load(&path).recent_repos, ["C:/git/one"]);
    assert!(!store.flush(), "clean again");

    // A store loaded from that file sees what was written.
    let reloaded = StateStore::load(Some(path.clone()));
    assert_eq!(reloaded.snapshot().recent_repos, ["C:/git/one"]);

    // A store with nowhere to write is harmless rather than a panic.
    let nowhere = StateStore::new(None);
    nowhere.update(|s| s.swapped = true);
    assert!(!nowhere.flush());
}

/// `settings.json` moved up one directory. An existing file at the old
/// `ProjectDirs` location is copied once, and never read again after that.
#[test]
fn settings_migrate_once_from_the_legacy_project_dirs_path() {
    use bondsymphonic_ide::qobjects::settings::{
        Settings, LEGACY_SETTINGS_PATH_ENV, SETTINGS_PATH_ENV, STATE_PATH_ENV,
    };
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = temp_dir("migrate");
    let new_path = dir.join("new").join("settings.json");
    let legacy_path = dir.join("old").join("settings.json");
    std::fs::create_dir_all(legacy_path.parent().expect("old dir")).expect("old dir");
    std::fs::write(
        &legacy_path,
        r#"{"distro":"legacy-distro","log_level":"debug"}"#,
    )
    .expect("legacy settings");

    std::env::set_var(SETTINGS_PATH_ENV, &new_path);
    std::env::set_var(LEGACY_SETTINGS_PATH_ENV, &legacy_path);
    std::env::remove_var(STATE_PATH_ENV);

    assert_eq!(Settings::path().as_deref(), Some(new_path.as_path()));

    // First load: the old file is copied up and read from its new home.
    let migrated = Settings::load();
    assert_eq!(migrated.distro, "legacy-distro");
    assert_eq!(migrated.log_level, "debug");
    assert!(new_path.exists(), "the settings file moved up a directory");
    assert!(legacy_path.exists(), "the old file is copied, not moved");

    // The new file is authoritative from now on: a later change to the old one
    // is not picked up, and saving does not write back to it.
    std::fs::write(&legacy_path, r#"{"distro":"changed-behind-our-back"}"#).expect("legacy");
    let mut settings = Settings::load();
    assert_eq!(settings.distro, "legacy-distro");
    settings.distro = "chosen".to_owned();
    settings.save().expect("settings save");
    assert_eq!(Settings::load().distro, "chosen");
    assert!(std::fs::read_to_string(&legacy_path)
        .expect("legacy")
        .contains("changed-behind-our-back"));

    std::env::remove_var(SETTINGS_PATH_ENV);
    std::env::remove_var(LEGACY_SETTINGS_PATH_ENV);
}

/// `state.json` sits beside `settings.json`, and `BS_STATE_PATH` overrides it
/// the way `BS_SETTINGS_PATH` overrides the settings file.
#[test]
fn the_state_file_sits_beside_the_settings_file_unless_overridden() {
    use bondsymphonic_ide::qobjects::settings::{Settings, SETTINGS_PATH_ENV, STATE_PATH_ENV};
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = temp_dir("state-path");
    let settings_path = dir.join("cfg").join("settings.json");
    std::env::set_var(SETTINGS_PATH_ENV, &settings_path);
    std::env::remove_var(STATE_PATH_ENV);

    assert_eq!(
        Settings::state_path().as_deref(),
        Some(dir.join("cfg").join("state.json").as_path())
    );

    let explicit = dir.join("elsewhere.json");
    std::env::set_var(STATE_PATH_ENV, &explicit);
    assert_eq!(Settings::state_path().as_deref(), Some(explicit.as_path()));

    std::env::remove_var(STATE_PATH_ENV);
    std::env::remove_var(SETTINGS_PATH_ENV);

    // With nothing overridden the two live in the same directory, and that
    // directory is the flattened one: %APPDATA%\BondSymphonic.
    let real_settings = Settings::path().expect("a config dir");
    let real_state = Settings::state_path().expect("a config dir");
    assert_eq!(real_settings.parent(), real_state.parent());
    assert_eq!(
        real_state.file_name().and_then(|n| n.to_str()),
        Some("state.json")
    );
    assert_eq!(
        real_settings
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str()),
        Some("BondSymphonic")
    );
}

/// `state.json` is written the way the daemon writes its own state files: a
/// unique sibling temporary, synced to the device, then a rename.
///
/// The uniqueness is not decoration. A fixed `state.json.tmp` is shared by
/// every writer of that file, so two IDE windows over the same `%APPDATA%`
/// path can rename each other's half-written temporary into place, and a
/// leftover directory or a read-only file at that name wedges the writer for
/// as long as it is there.
#[test]
fn saves_go_through_a_unique_temporary_and_leave_nothing_behind() {
    let dir = temp_dir("atomic");
    let path = dir.join("state.json");

    let mut state = StateFile::default();
    state.note_recent("C:/git/one");
    save(&path, &state).expect("first save");
    state.note_recent("C:/git/two");
    save(&path, &state).expect("second save");

    assert_eq!(load(&path).recent_repos, ["C:/git/two", "C:/git/one"]);

    // Two saves in a row and the directory holds the state file and nothing
    // else: no temporary survived either of them.
    let left: Vec<String> = std::fs::read_dir(&dir)
        .expect("read the directory back")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        left,
        ["state.json"],
        "a temporary was left behind: {left:?}"
    );

    // The name carries this process's id and a counter, so it cannot collide
    // with another process's temporary or with the writer's own next one.
    let pid = std::process::id();
    let first = temp_path(&path);
    let second = temp_path(&path);
    assert_ne!(first, second, "two calls must not produce one name");
    for tmp in [&first, &second] {
        assert_eq!(tmp.parent(), path.parent(), "the temporary is a sibling");
        let name = tmp
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        let rest = name
            .strip_prefix(&format!(".state.json.{pid}."))
            .unwrap_or_else(|| panic!("unexpected temporary name {name:?}"));
        let counter = rest
            .strip_suffix(".tmp")
            .unwrap_or_else(|| panic!("unexpected temporary name {name:?}"));
        assert!(
            counter.chars().all(|c| c.is_ascii_digit()) && !counter.is_empty(),
            "the counter is not a number in {name:?}"
        );
    }
}

/// A `settings.json` that will not parse is kept, never overwritten.
///
/// The old `load` swallowed the parse error and answered with the defaults, and
/// the next read-modify-write -- recording that an API key had been stored, say
/// -- wrote those defaults straight over the file. One typo in a hand-edited
/// settings file cost the user every setting in it, silently.
#[test]
fn a_malformed_settings_file_is_kept_aside_and_never_overwritten() {
    use bondsymphonic_ide::qobjects::settings::{
        Settings, SettingsError, LEGACY_SETTINGS_PATH_ENV, SETTINGS_PATH_ENV, STATE_PATH_ENV,
    };
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = temp_dir("malformed-settings");
    let path = dir.join("settings.json");
    // A trailing comma: the commonest way a hand-edited JSON file stops parsing.
    let original = r#"{"distro":"mine","log_level":"debug",}"#;
    std::fs::write(&path, original).expect("a malformed settings file");
    std::env::set_var(SETTINGS_PATH_ENV, &path);
    std::env::remove_var(LEGACY_SETTINGS_PATH_ENV);
    std::env::remove_var(STATE_PATH_ENV);

    let backup = match Settings::try_load() {
        Err(SettingsError::Malformed {
            path: reported,
            backup,
        }) => {
            assert_eq!(reported, path);
            backup.expect("the unreadable file was moved aside")
        }
        other => panic!("a file that will not parse must not read as settings: {other:?}"),
    };
    assert!(!path.exists(), "the unreadable file is out of the way");
    assert_eq!(
        std::fs::read_to_string(&backup).expect("the kept copy"),
        original,
        "the user's own bytes, untouched"
    );
    assert!(
        backup
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("settings.json.bad-")),
        "kept as {}",
        backup.display()
    );

    // And the read-modify-write that used to flatten it writes nothing at all.
    std::fs::write(&path, original).expect("a second malformed settings file");
    Settings::record_api_key_set(true);
    assert!(
        !path.exists(),
        "record_api_key_set wrote a defaults file over settings it could not read"
    );

    // With no settings file at all there is nothing to lose, so the same call
    // does write one: this is a first run, not a damaged file.
    Settings::record_api_key_set(true);
    let written = std::fs::read_to_string(&path).expect("a fresh settings file");
    assert!(
        written.contains("\"api_key_set\": true"),
        "the fresh file records the key: {written}"
    );
    assert!(
        written.contains("\"distro\""),
        "and is a whole settings object: {written}"
    );

    std::env::remove_var(SETTINGS_PATH_ENV);
}

/// Task 15 (CI1): an arrangement that has not moved is not written.
///
/// Every status glyph, agent heartbeat and running cost moves the tab model,
/// and the window reports the arrangement back on the signal that says so.
/// `GroupModel` now fires `arrangementChanged` only when the groups themselves
/// moved, but the store is the other half of the same guarantee: a report that
/// says exactly what the file already holds must not mark it dirty, because
/// dirty is what schedules an `fsync` of `state.json`.
#[test]
fn an_unchanged_arrangement_neither_dirties_the_store_nor_writes_the_file() {
    let dir = temp_dir("unchanged-arrangement");
    let path = dir.join("state.json");
    let store = StateStore::new(Some(path.clone()));

    let groups = || {
        vec![PersistedGroup {
            name: "Feature A".to_owned(),
            workspace_ids: vec!["ws_1".to_owned(), "ws_2".to_owned()],
            tabs: Vec::new(),
        }]
    };

    // The first report is news and is written.
    let token = store.update_changed(|s| s.set_groups(groups(), Some("ws_1".to_owned())));
    assert!(token.is_some(), "a first arrangement is a change");
    assert!(store.flush(), "and is written");
    let written = std::fs::read_to_string(&path).expect("state.json");
    assert!(written.contains("Feature A"));

    // The same arrangement again is not.
    assert!(
        store
            .update_changed(|s| s.set_groups(groups(), Some("ws_1".to_owned())))
            .is_none(),
        "an identical arrangement is not a change"
    );
    assert!(
        !store.flush(),
        "and leaves nothing to write, so no fsync is scheduled"
    );

    // A real move is, and so is a change of the active workspace on its own.
    assert!(store
        .update_changed(|s| s.set_groups(groups(), Some("ws_2".to_owned())))
        .is_some());
    assert!(store.flush());

    let mut moved = groups();
    moved[0].workspace_ids.reverse();
    assert!(store
        .update_changed(|s| s.set_groups(moved, Some("ws_2".to_owned())))
        .is_some());
    assert!(store.flush());

    let _ = std::fs::remove_dir_all(&dir);
}

/// A settings file written before the mode list was corrected holds
/// `"default"`, which the pinned CLI rejects -- so every agent that user starts
/// dies of a usage error. Reading it back as `manual` is the only repair that
/// does not require them to find the setting that is poisoning their IDE.
#[test]
fn a_stored_default_permission_mode_reads_back_as_manual() {
    use bondsymphonic_ide::qobjects::settings::{
        Settings, LEGACY_SETTINGS_PATH_ENV, SETTINGS_PATH_ENV, STATE_PATH_ENV,
    };
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = temp_dir("permission-mode");
    let path = dir.join("settings.json");
    std::fs::write(&path, r#"{"default_permission_mode":"default"}"#).expect("settings");
    std::env::set_var(SETTINGS_PATH_ENV, &path);
    std::env::remove_var(LEGACY_SETTINGS_PATH_ENV);
    std::env::remove_var(STATE_PATH_ENV);

    assert_eq!(Settings::load().default_permission_mode, "manual");

    // A mode the CLI does take is left exactly as it was.
    std::fs::write(&path, r#"{"default_permission_mode":"plan"}"#).expect("settings");
    assert_eq!(Settings::load().default_permission_mode, "plan");

    std::env::remove_var(SETTINGS_PATH_ENV);
}

/// The palette is a setting like any other: chosen once, remembered, and
/// absent from a file written before it existed -- which must read back as
/// "follow the system" rather than as an empty string nothing can apply.
#[test]
fn the_theme_choice_round_trips_and_defaults_to_the_system_one() {
    use bondsymphonic_ide::qobjects::settings::{
        Settings, LEGACY_SETTINGS_PATH_ENV, SETTINGS_PATH_ENV, STATE_PATH_ENV,
    };
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = temp_dir("theme");
    let path = dir.join("settings.json");
    std::fs::write(&path, r#"{"distro":"bondsymphonic"}"#).expect("settings");
    std::env::set_var(SETTINGS_PATH_ENV, &path);
    std::env::remove_var(LEGACY_SETTINGS_PATH_ENV);
    std::env::remove_var(STATE_PATH_ENV);

    let mut settings = Settings::load();
    assert_eq!(settings.theme, "system");

    settings.theme = "light".to_owned();
    settings.save().expect("settings save");
    assert_eq!(Settings::load().theme, "light");

    std::env::remove_var(SETTINGS_PATH_ENV);
}

/// The turn cost and the agent's own system lines are on until someone says
/// otherwise, so a settings file that predates the switch reads back as
/// showing them -- an absent field is not an answer of "hide".
#[test]
fn the_small_grey_lines_round_trip_and_default_to_shown() {
    use bondsymphonic_ide::qobjects::settings::{
        Settings, LEGACY_SETTINGS_PATH_ENV, SETTINGS_PATH_ENV, STATE_PATH_ENV,
    };
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = temp_dir("show-agent-meta");
    let path = dir.join("settings.json");
    std::fs::write(&path, r#"{"distro":"bondsymphonic"}"#).expect("settings");
    std::env::set_var(SETTINGS_PATH_ENV, &path);
    std::env::remove_var(LEGACY_SETTINGS_PATH_ENV);
    std::env::remove_var(STATE_PATH_ENV);

    let mut settings = Settings::load();
    assert!(settings.show_agent_meta);

    settings.show_agent_meta = false;
    settings.save().expect("settings save");
    assert!(!Settings::load().show_agent_meta);

    std::env::remove_var(SETTINGS_PATH_ENV);
}
