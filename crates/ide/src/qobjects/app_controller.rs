use crate::model::app_state::ConnectionState;

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
        self.set_status_message(QString::from(ConnectionState::Disconnected.label()));
    }

    pub fn set_state(self: Pin<&mut Self>, state: ConnectionState) {
        let mut this = self;
        this.as_mut().set_connection_state(state.as_i32());
        this.as_mut()
            .set_status_message(QString::from(state.label()));
    }
}
