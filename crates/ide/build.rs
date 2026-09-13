use cxx_qt_build::CxxQtBuilder;

fn main() {
    embed_windows_icon();
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    // The offscreen widget checks at the foot of several .cpp files -- they
    // build widgets, leak a QApplication and assert -- are test code, and the
    // shipped IDE has no caller for any of them. `cargo test` builds the dev
    // profile, so keying on the profile compiles them for every run of the
    // suites and for none of the packaged executables.
    let widget_tests = std::env::var("PROFILE").as_deref() != Ok("release");
    let builder = CxxQtBuilder::new();
    // SAFETY: the closure only adds a preprocessor define; it does not change
    // include paths, flags or files in a way that could conflict with cxx-qt.
    let builder = unsafe {
        builder.cc_builder(move |cc| {
            cc.define("BS_IDE_VERSION", format!("\"{version}\"").as_str());
            if widget_tests {
                cc.define("BS_WIDGET_TESTS", None);
            }
            // MSVC otherwise reads these sources in the system code page, and
            // the glyphs in the setup page's rows are UTF-8 in the file.
            cc.flag_if_supported("/utf-8");
        })
    };
    builder
        // Widgets only. Gui is a hard dependency of Widgets (QAction, QMetaObject
        // for QAction, ...) and is not linked implicitly by cxx-qt-build.
        .qt_module("Gui")
        .qt_module("Widgets")
        .file("src/ffi.rs")
        .file("src/qobjects/app_controller.rs")
        .file("src/qobjects/group_model.rs")
        .file("src/qobjects/file_tree.rs")
        .file("src/qobjects/terminal_session.rs")
        .file("src/qobjects/editor_document.rs")
        .file("src/qobjects/diff_document.rs")
        .file("src/qobjects/changes_model.rs")
        .file("src/qobjects/transcript_model.rs")
        .file("src/qobjects/run_panel.rs")
        // Not a QObject: three free functions the `BS_MENU_TEST` seam in
        // `GroupBar` and `MainWindow` calls, so the model fixtures a menu step
        // installs are built in Rust rather than in a widget.
        .file("src/qobjects/smoke.rs")
        // Headers are run through moc, sources are compiled.
        .cpp_files([
            "cpp/AgentChoices.cpp",
            "cpp/GroupBar.h",
            "cpp/GroupBar.cpp",
            "cpp/TerminalWidget.h",
            "cpp/TerminalWidget.cpp",
            "cpp/AgentArea.h",
            "cpp/AgentArea.cpp",
            "cpp/ExplorerDock.h",
            "cpp/ExplorerDock.cpp",
            "cpp/ChangesToolbar.h",
            "cpp/ChangesToolbar.cpp",
            "cpp/PrDialog.h",
            "cpp/PrDialog.cpp",
            "cpp/CloseGroupDialog.h",
            "cpp/CloseGroupDialog.cpp",
            "cpp/WorkspaceBanner.h",
            "cpp/WorkspaceBanner.cpp",
            "cpp/NewAgentDialog.h",
            "cpp/NewAgentDialog.cpp",
            "cpp/SetupPage.h",
            "cpp/SetupPage.cpp",
            "cpp/SettingsDialog.h",
            "cpp/SettingsDialog.cpp",
            "cpp/PromptInput.h",
            "cpp/PromptInput.cpp",
            "cpp/ToolCard.h",
            "cpp/ToolCard.cpp",
            "cpp/PermissionBar.h",
            "cpp/PermissionBar.cpp",
            "cpp/TranscriptView.h",
            "cpp/TranscriptView.cpp",
            "cpp/CodeView.h",
            "cpp/CodeView.cpp",
            "cpp/RustHighlighter.h",
            "cpp/RustHighlighter.cpp",
            "cpp/EditorWidget.h",
            "cpp/EditorWidget.cpp",
            "cpp/EditorArea.h",
            "cpp/EditorArea.cpp",
            "cpp/DiffWidget.h",
            "cpp/DiffWidget.cpp",
            "cpp/RunPanel.h",
            "cpp/RunPanel.cpp",
            "cpp/MainWindow.h",
            "cpp/MainWindow.cpp",
            "cpp/app.cpp",
            "cpp/Branding.cpp",
        ])
        .build();
    // cxx-qt-build emits a rerun line for every file it is handed, which covers
    // `cpp_files` and the bridges above. These are included by those files but
    // named in no list, so without this a change to the one header that defines
    // every accent colour would leave the build stale.
    for header in [
        "cpp/Theme.h",
        "cpp/AgentChoices.h",
        "cpp/Branding.h",
        "cpp/LogoData.h",
        "cpp/app.h",
    ] {
        println!("cargo:rerun-if-changed={header}");
    }
}

/// Stamp the logo into the executable's resources so Explorer and the taskbar
/// show it. A missing resource compiler must not break the build: the window
/// icon set at runtime still works, only the .exe file icon is lost.
#[cfg(target_os = "windows")]
fn embed_windows_icon() {
    println!("cargo:rerun-if-changed=../../assets/logo.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../assets/logo.ico");
    if let Err(err) = res.compile() {
        println!("cargo:warning=executable icon not embedded: {err}");
    }
}

#[cfg(not(target_os = "windows"))]
fn embed_windows_icon() {}
