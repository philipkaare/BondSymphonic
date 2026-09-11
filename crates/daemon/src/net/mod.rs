pub mod allowlist;
/// The host half of the port bridge. Unix sockets only, which is also the only
/// place a sandbox with a network namespace of its own exists.
#[cfg(unix)]
pub mod bridge;
pub mod forward;
/// The HTTP message plumbing [`proxy`] is built on. Private: the daemon has one
/// HTTP proxy and nothing else has any business parsing request heads.
mod http;
pub mod proxy;
pub mod shim;
