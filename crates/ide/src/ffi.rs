// The inner module name is part of the public path `bondsymphonic_ide::ffi::ffi`,
// which the C++ entry point is referenced by, so keep it despite the lint.
#[allow(clippy::module_inception)]
#[cxx::bridge]
pub mod ffi {
    unsafe extern "C++" {
        include!("bondsymphonic-ide/cpp/app.h");
        /// Creates QApplication + MainWindow and runs the event loop. Returns the exit code.
        fn run_app() -> i32;
    }
}
