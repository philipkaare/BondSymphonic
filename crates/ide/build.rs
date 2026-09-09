use cxx_qt_build::CxxQtBuilder;

fn main() {
    CxxQtBuilder::new()
        // Widgets only. Gui is a hard dependency of Widgets (QAction, QMetaObject
        // for QAction, ...) and is not linked implicitly by cxx-qt-build.
        .qt_module("Gui")
        .qt_module("Widgets")
        .file("src/ffi.rs")
        .file("src/qobjects/app_controller.rs")
        .file("src/qobjects/group_model.rs")
        .file("src/qobjects/file_tree.rs")
        .file("src/qobjects/terminal_session.rs")
        // Headers are run through moc, sources are compiled.
        .cpp_files([
            "cpp/GroupBar.h",
            "cpp/GroupBar.cpp",
            "cpp/TerminalWidget.h",
            "cpp/TerminalWidget.cpp",
            "cpp/AgentArea.h",
            "cpp/AgentArea.cpp",
            "cpp/NewAgentDialog.h",
            "cpp/NewAgentDialog.cpp",
            "cpp/MainWindow.h",
            "cpp/MainWindow.cpp",
            "cpp/app.cpp",
        ])
        .build();
}
