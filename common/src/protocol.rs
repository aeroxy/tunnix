use serde::{Deserialize, Serialize};

/// Message types exchanged between client and server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    /// Client requests connection to target host
    Connect {
        conn_id: u32,
        host: String,
        port: u16,
    },

    /// Encrypted data payload
    Data {
        conn_id: u32,
        data: Vec<u8>,
    },

    /// Close specific connection
    Close {
        conn_id: u32,
    },

    /// Keep-alive ping
    Ping,

    /// Keep-alive pong response
    Pong,

    /// Error response
    Error {
        conn_id: Option<u32>,
        message: String,
    },
}

/// Frame format for encrypted messages
///
/// Wire format:
/// [4 bytes: payload length][12 bytes: nonce][N bytes: encrypted payload][16 bytes: auth tag]
#[derive(Debug)]
pub struct Frame {
    pub payload: Vec<u8>,
}

impl Frame {
    /// Create a new frame from serialized message
    pub fn new(payload: Vec<u8>) -> Self {
        Self { payload }
    }

    /// Get payload as slice
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Consume frame and return payload
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }
}

impl Message {
    /// Serialize message to bytes using bincode
    pub fn to_bytes(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    /// Deserialize message from bytes
    pub fn from_bytes(data: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_serialization() {
        let msg = Message::Connect {
            conn_id: 123,
            host: "example.com".to_string(),
            port: 443,
        };

        let bytes = msg.to_bytes().unwrap();
        let decoded = Message::from_bytes(&bytes).unwrap();

        match decoded {
            Message::Connect { conn_id, host, port } => {
                assert_eq!(conn_id, 123);
                assert_eq!(host, "example.com");
                assert_eq!(port, 443);
            }
            _ => panic!("Wrong message type"),
        }
    }

    #[test]
    fn test_data_message() {
        let data = vec![1, 2, 3, 4, 5];
        let msg = Message::Data {
            conn_id: 456,
            data: data.clone(),
        };

        let bytes = msg.to_bytes().unwrap();
        let decoded = Message::from_bytes(&bytes).unwrap();

        match decoded {
            Message::Data { conn_id, data: decoded_data } => {
                assert_eq!(conn_id, 456);
                assert_eq!(decoded_data, data);
            }
            _ => panic!("Wrong message type"),
        }
    }
}
