use cxx_qt_build::CxxQtBuilder;

fn main() {
    embed_windows_icon();
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let builder = CxxQtBuilder::new();
    // SAFETY: the closure only adds a preprocessor define; it does not change
    // include paths, flags or files in a way that could conflict with cxx-qt.
    let builder = unsafe {
        builder.cc_builder(move |cc| {
            cc.define("BS_IDE_VERSION", format!("\"{version}\"").as_str());
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
        // Headers are run through moc, sources are compiled.
        .cpp_files([
            "cpp/GroupBar.h",
            "cpp/GroupBar.cpp",
            "cpp/TerminalWidget.h",
            "cpp/TerminalWidget.cpp",
            "cpp/AgentArea.h",
            "cpp/AgentArea.cpp",
            "cpp/ExplorerDock.h",
            "cpp/ExplorerDock.cpp",
            "cpp/NewAgentDialog.h",
            "cpp/NewAgentDialog.cpp",
            "cpp/CodeView.h",
            "cpp/CodeView.cpp",
            "cpp/RustHighlighter.h",
            "cpp/RustHighlighter.cpp",
            "cpp/EditorWidget.h",
            "cpp/EditorWidget.cpp",
            "cpp/EditorArea.h",
            "cpp/EditorArea.cpp",
            "cpp/MainWindow.h",
            "cpp/MainWindow.cpp",
            "cpp/app.cpp",
            "cpp/Branding.cpp",
        ])
        .build();
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
