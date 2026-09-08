use crate::model::app_state::{compose_status, ConnectionState};

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    // `auto_cxx_name` maps snake_case Rust names onto camelCase C++ names, so
    // `status_message` is exposed as `getStatusMessage`/`statusMessageChanged`.
    #[auto_cxx_name]
    unsafe extern "RustQt" {
        #[qobject]
        #[qproperty(i32, connection_state)]
        #[qproperty(QString, status_message)]
        #[qproperty(QString, daemon_version)]
        type AppController = super::AppControllerRust;

        #[qsignal]
        fn prereq_warning(self: Pin<&mut AppController>, message: QString);

        /// Launch the daemon and connect. Filled in by Task 9.
        #[qinvokable]
        fn start(self: Pin<&mut AppController>);

        /// Record the daemon version and recompose the status text. Callers use
        /// this rather than `set_daemon_version` so the status bar stays in sync.
        #[qinvokable]
        fn apply_daemon_version(self: Pin<&mut AppController>, version: QString);
    }

    impl cxx_qt::Threading for AppController {}
}

use core::pin::Pin;
use cxx_qt_lib::QString;

pub struct AppControllerRust {
    connection_state: i32,
    status_message: QString,
    daemon_version: QString,
}

impl Default for AppControllerRust {
    fn default() -> Self {
        Self {
            connection_state: ConnectionState::Disconnected.as_i32(),
            status_message: QString::from(ConnectionState::Disconnected.label()),
            daemon_version: QString::from(""),
        }
    }
}

impl qobject::AppController {
    pub fn start(self: Pin<&mut Self>) {
        self.set_state(ConnectionState::Disconnected);
    }

    pub fn set_state(mut self: Pin<&mut Self>, state: ConnectionState) {
        self.as_mut().set_connection_state(state.as_i32());
        self.refresh_status(state);
    }

    pub fn apply_daemon_version(mut self: Pin<&mut Self>, version: QString) {
        self.as_mut().set_daemon_version(version);
        let state = ConnectionState::from_i32(*self.connection_state());
        self.refresh_status(state);
    }

    /// Recomposes `status_message` from `state` and the current `daemon_version`.
    fn refresh_status(mut self: Pin<&mut Self>, state: ConnectionState) {
        let version = self.daemon_version().to_string();
        let text = compose_status(state.label(), &version);
        self.as_mut().set_status_message(QString::from(&text));
    }
}
