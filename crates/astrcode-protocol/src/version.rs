//! Protocol version negotiation.

use serde::{Deserialize, Serialize};

/// Initial handshake from client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeRequest {
    pub protocol_version: u32,
    pub client_info: ClientInfo,
}

/// Server response to initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResponse {
    pub accepted_version: u32,
    pub server_info: ServerInfo,
}

/// Information about the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

/// Information about the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub protocol_versions: Vec<u32>,
    pub capabilities: ServerCapabilities,
}

/// Server capability flags.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerCapabilities {
    pub streaming: bool,
    pub session_fork: bool,
    pub compaction: bool,
    pub extensions: bool,
}

/// Negotiate protocol version between client and server.
///
/// Returns the highest version both support, or None if incompatible.
pub fn negotiate_version(client_requested: u32, server_supported: &[u32]) -> Option<u32> {
    if server_supported.contains(&client_requested) {
        return Some(client_requested);
    }
    // Find the highest version both support
    server_supported
        .iter()
        .copied()
        .filter(|v| *v <= client_requested)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_negotiate_exact_match() {
        let result = negotiate_version(1, &[1, 2]);
        assert_eq!(result, Some(1));
    }

    #[test]
    fn test_negotiate_highest_compatible() {
        let result = negotiate_version(3, &[1, 2]);
        assert_eq!(result, Some(2));
    }

    #[test]
    fn test_negotiate_incompatible() {
        let result = negotiate_version(1, &[2, 3]);
        assert_eq!(result, None);
    }
}
