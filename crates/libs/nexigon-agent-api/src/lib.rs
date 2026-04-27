//! API definitions for the Nexigon Agent local API.
//!
//! The agent listens on a Unix socket and accepts connections that carry a
//! length-prefixed JSON handshake. Each connection then runs whichever
//! endpoint protocol the [`types::handshake::ClientHello`] requested.
//!
//! Wire format on the socket, both directions:
//!
//! ```text
//! [ 4 bytes MAGIC ][ u32 BE length ][ JSON payload ]
//! ```

sidex::include_bundle!(
    #[allow(warnings)]
    pub nexigon_agent_api as types
);

#[cfg(unix)]
pub mod client;

/// Magic prefix identifying the agent local API protocol on the wire.
///
/// Sent by both sides as the first four bytes of the connection. A
/// mismatch indicates the peer is not speaking this protocol.
pub const MAGIC: [u8; 4] = *b"NXAC";

/// Maximum size, in bytes, of a single handshake JSON payload.
///
/// This bounds the parser against pathological inputs from a peer that
/// passed the [`MAGIC`] check.
pub const MAX_HANDSHAKE_LEN: u32 = 64 * 1024;

/// Current protocol version.
pub const VERSION: u32 = 1;

/// Default path of the agent's local API Unix socket.
pub const DEFAULT_SOCKET_PATH: &str = "/run/nexigon/agent/control/socket.sock";
