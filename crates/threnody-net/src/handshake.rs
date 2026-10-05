//! Drives the core handshake over any framed byte stream.

use std::time::Duration;

use threnody_core::handshake::{Initiator, Responder};
use threnody_core::{Identity, SecureChannel};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::{NetError, Result};
use crate::frame::{read_frame, write_frame};

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

async fn next<S: AsyncRead + Unpin>(s: &mut S) -> Result<Vec<u8>> {
    read_frame(s).await?.ok_or(NetError::Closed)
}

/// Runs the initiator side to completion.
pub async fn initiate<S>(s: &mut S, identity: &Identity) -> Result<SecureChannel>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let (ini, hs1) = Initiator::start(identity)?;
        write_frame(s, &hs1).await?;
        let hs2 = next(s).await?;
        let (hs3, est) = ini.finish(&hs2)?;
        write_frame(s, &hs3).await?;
        Ok(SecureChannel::from(est))
    })
    .await
    .map_err(|_| NetError::Timeout)?
}

/// Runs the responder side to completion.
pub async fn accept<S>(s: &mut S, identity: &Identity) -> Result<SecureChannel>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let hs1 = tokio::time::timeout(HANDSHAKE_TIMEOUT, next(s))
        .await
        .map_err(|_| NetError::Timeout)??;
    accept_first(s, identity, &hs1).await
}

/// Runs the responder side when the initiator's first frame was already
/// read (to tell an anonymous link from a session; see `anon`).
pub async fn accept_first<S>(s: &mut S, identity: &Identity, hs1: &[u8]) -> Result<SecureChannel>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let (resp, hs2) = Responder::respond(identity, hs1)?;
        write_frame(s, &hs2).await?;
        let hs3 = next(s).await?;
        Ok(SecureChannel::from(resp.finish(&hs3)?))
    })
    .await
    .map_err(|_| NetError::Timeout)?
}
