//! No test may write the developer's real `%LOCALAPPDATA%\BondSymphonic\logs`.
//!
//! Every launch of the IDE binary opens `ide.log` and rotates the previous one
//! out before Qt even starts, so a test that launched the IDE without
//! `BS_LOG_DIR` threw away the user's last real launch log -- the one file a
//! bug report needs. The launches all set `BS_STATE_PATH` already, for the
//! same reason; this checks, cheaply and without Qt, that each one that does
//! also sets `BS_LOG_DIR`.

use std::path::Path;

#[test]
fn every_test_that_launches_the_ide_points_its_log_dir_at_a_temp_dir() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut checked = 0;
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(&tests).expect("crates/ide/tests") {
        let path = entry.expect("a test file").path();
        if path.extension().is_none_or(|e| e != "rs") || path.ends_with("log_isolation.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("a readable test file");
        // A launch of the built binary, or of the packaged exe, which sets
        // `BS_STATE_PATH` like every other launch does.
        let launches = text
            .matches("CARGO_BIN_EXE_bondsymphonic-ide")
            .count()
            .max(text.matches(".env(\"BS_STATE_PATH\"").count());
        let isolated = text.matches(".env(\"BS_LOG_DIR\"").count();
        checked += launches;
        if isolated < launches {
            missing.push(format!(
                "{}: {launches} launch(es), {isolated} BS_LOG_DIR",
                path.display()
            ));
        }
    }
    assert!(checked > 0, "found no IDE launches to check");
    assert!(
        missing.is_empty(),
        "these tests launch the IDE without BS_LOG_DIR, so it logs into the \
         user's real %LOCALAPPDATA%\\BondSymphonic\\logs:\n{}",
        missing.join("\n")
    );
}
