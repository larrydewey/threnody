use thiserror::Error;

/// Every failure the protocol core can report.
///
/// Variants deliberately carry little detail: callers must treat any
/// cryptographic failure as "drop the message / abort the session" and
/// must not leak which check failed to the network.
#[derive(Debug, Error)]
pub enum Error {
    #[error("malformed CBOR: {0}")]
    Cbor(&'static str),
    #[error("malformed message: {0}")]
    Malformed(&'static str),
    #[error("unsupported protocol major version {0}")]
    UnsupportedVersion(u64),
    #[error("unexpected message type {0}")]
    UnexpectedType(u64),
    #[error("no mutually supported cipher suite")]
    NoCommonSuite,
    #[error("invalid public key")]
    InvalidKey,
    #[error("signature verification failed")]
    BadSignature,
    #[error("decryption failed")]
    Decrypt,
    #[error("too many skipped messages")]
    TooManySkipped,
    #[error("message key already used or expired")]
    Replay,
    #[error("ratchet cannot send yet: waiting for the peer's first message")]
    NotReady,
    #[error("this identity is protected by a passphrase")]
    PassphraseRequired,
    #[error("invalid fingerprint")]
    InvalidFingerprint,
    #[error("storage: {0}")]
    Io(#[from] std::io::Error),
}

impl From<const_cbor::Error> for Error {
    fn from(e: const_cbor::Error) -> Self {
        Self::Cbor(e.name())
    }
}

pub type Result<T> = core::result::Result<T, Error>;
