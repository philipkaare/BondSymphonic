pub mod codec;
pub mod error;
pub mod event;
pub mod ids;
pub mod message;
pub mod request;
pub mod types;
pub mod workspace_name;

pub use error::*;
pub use event::*;
pub use ids::*;
pub use message::*;
pub use request::*;
pub use types::*;

/// The wire protocol this build speaks. Carried both ways by `hello`: the IDE
/// and the daemon are shipped as a pair, and a pair that does not match is
/// told so at the handshake rather than failing later on a field one side has
/// never heard of.
pub const PROTOCOL_VERSION: u32 = 1;

/// What a peer that sends no `protocol_version` at all is speaking.
///
/// The field was added in Milestone 7, after version 1 had already shipped in
/// both directions, so its absence is not "unknown" but "version 1".
pub const PRE_M7_PROTOCOL_VERSION: u32 = 1;

/// The version a peer is speaking, given the `protocol_version` it sent.
pub fn peer_protocol_version(sent: Option<u32>) -> u32 {
    sent.unwrap_or(PRE_M7_PROTOCOL_VERSION)
}
