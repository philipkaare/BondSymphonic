//! Runs: the commands a workspace can start, where they come from
//! ([`config`]) and the manager that starts them, streams their output and
//! bridges their port back to the host ([`manager`]).

pub mod config;
pub mod manager;
