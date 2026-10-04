use thiserror::Error;

#[derive(Debug, Error)]
pub enum NetError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(#[from] threnody_core::Error),
    #[error("frame exceeds maximum size")]
    FrameTooLarge,
    #[error("connection closed (or peer not connected)")]
    Closed,
    #[error("handshake timed out")]
    Timeout,
    #[error("peer identity {got} does not match expected {expected}")]
    IdentityMismatch { expected: String, got: String },
    #[error("peer {0} refused by local policy")]
    Refused(String),
    #[error("no relay path to {0} (relays need mutual approval with both ends)")]
    NoRoute(String),
    #[error("node has shut down")]
    Shutdown,
}

pub type Result<T> = core::result::Result<T, NetError>;
