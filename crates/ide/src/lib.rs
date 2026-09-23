pub mod client;
pub mod ffi;
pub mod highlight;
pub mod launcher;
pub mod logfile;
pub mod model;
pub mod qobjects;
/// The Qt-less skip the integration tests share.
///
/// `pub` because integration tests are separate crates and cannot reach a
/// `#[cfg(test)]` module; `#[doc(hidden)]` because it is test scaffolding and
/// has no business in the crate's documented API. One small function, and the
/// alternative -- the same guard copy-pasted into four suites, which is what
/// this replaced -- is how they drifted apart in the first place.
#[doc(hidden)]
pub mod testing;
