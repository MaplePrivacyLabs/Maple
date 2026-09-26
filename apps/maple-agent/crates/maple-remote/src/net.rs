//! What both network roles share: the WebSocket configuration and the
//! handshake budget. The host role is [`crate::listen`], the client role
//! [`crate::dial`].

use std::time::Duration;

use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// Time a peer gets to finish the WebSocket and Noise handshakes.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Largest WebSocket message either side reads. A Noise message is at
/// most 65535 bytes, so anything larger is not this protocol and is
/// refused before it is buffered.
pub const MAX_WEBSOCKET_MESSAGE_BYTES: usize = 65535;

/// The WebSocket configuration both roles use.
pub fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_WEBSOCKET_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_WEBSOCKET_MESSAGE_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_messages_are_capped_at_one_noise_message() {
        let config = websocket_config();
        assert_eq!(config.max_message_size, Some(65535));
        assert_eq!(config.max_frame_size, Some(65535));
    }
}
