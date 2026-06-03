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

    /// Server tells client the session was freshly created (e.g. after a server
    /// restart). Client must treat any existing conn_ids as orphaned and tear
    /// them down — the server knows nothing about them.
    Reset,

    /// Client asks the server to open a PTY for this conn_id and run a command.
    /// cmd = None => interactive login shell ($SHELL or /bin/sh); Some(s) => `sh -c s`.
    /// cols/rows are the initial terminal size. The PTY's byte stream then flows
    /// over the existing Data/Close messages, exactly like a TCP connection.
    Exec {
        conn_id: u32,
        cmd: Option<String>,
        cols: u16,
        rows: u16,
        /// Client's $TERM, so the remote PTY advertises the right terminal type
        /// (falls back to xterm-256color client-side when unset).
        term: String,
    },

    /// Client's terminal was resized; server applies the new size to the PTY.
    Resize {
        conn_id: u32,
        cols: u16,
        rows: u16,
    },

    /// Server reports the child process exit code (sent just before Close).
    ExitStatus {
        conn_id: u32,
        code: i32,
    },
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

    #[test]
    fn test_exec_messages() {
        for msg in [
            Message::Exec { conn_id: 7, cmd: Some("ls /home".into()), cols: 80, rows: 24, term: "xterm-256color".into() },
            Message::Exec { conn_id: 8, cmd: None, cols: 120, rows: 40, term: "screen-256color".into() },
            Message::Resize { conn_id: 7, cols: 100, rows: 30 },
            Message::ExitStatus { conn_id: 7, code: 42 },
        ] {
            let bytes = msg.to_bytes().unwrap();
            let decoded = Message::from_bytes(&bytes).unwrap();
            match (msg, decoded) {
                (
                    Message::Exec { conn_id: a, cmd: c1, cols: cl1, rows: r1, term: t1 },
                    Message::Exec { conn_id: b, cmd: c2, cols: cl2, rows: r2, term: t2 },
                ) => {
                    assert_eq!(a, b);
                    assert_eq!(c1, c2);
                    assert_eq!((cl1, r1), (cl2, r2));
                    assert_eq!(t1, t2);
                }
                (
                    Message::Resize { conn_id: a, cols: cl1, rows: r1 },
                    Message::Resize { conn_id: b, cols: cl2, rows: r2 },
                ) => {
                    assert_eq!(a, b);
                    assert_eq!((cl1, r1), (cl2, r2));
                }
                (
                    Message::ExitStatus { conn_id: a, code: c1 },
                    Message::ExitStatus { conn_id: b, code: c2 },
                ) => {
                    assert_eq!(a, b);
                    assert_eq!(c1, c2);
                }
                _ => panic!("Wrong message type"),
            }
        }
    }
}
