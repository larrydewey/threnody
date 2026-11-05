//! Drives the core handshake over any framed byte stream.

use std::time::Duration;

use threnody_core::handshake::{Initiator, Responder};
use threnody_core::{Fingerprint, Identity, SecureChannel};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::{NetError, Result};
use crate::frame::{MAX_HANDSHAKE_FRAME, read_frame_max, write_frame};

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Reads one handshake frame.
pub(crate) async fn next<S: AsyncRead + Unpin>(s: &mut S) -> Result<Vec<u8>> {
    read_frame_max(s, MAX_HANDSHAKE_FRAME)
        .await?
        .ok_or(NetError::Closed)
}

/// Runs the initiator side to completion. With `expect`, the responder must
/// present that fingerprint; anyone else is refused before HS3, so they
/// never learn who was dialing.
pub async fn initiate<S>(
    s: &mut S,
    identity: &Identity,
    expect: Option<Fingerprint>,
) -> Result<SecureChannel>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let (ini, hs1) = Initiator::start(identity)?;
        write_frame(s, &hs1).await?;
        let hs2 = next(s).await?;
        let auth = ini.authenticate(&hs2)?;
        let got = auth.peer().fingerprint();
        if let Some(want) = expect
            && got != want
        {
            return Err(NetError::IdentityMismatch {
                expected: want.to_string(),
                got: got.to_string(),
            });
        }
        let (hs3, est) = auth.finish()?;
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

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[tokio::test]
    async fn oversized_handshake_frames_are_refused_unread() {
        let me = Identity::generate();
        let (mut ours, mut theirs) = tokio::io::duplex(1 << 16);
        // A stranger announces a 16 MiB first frame and then stalls.
        theirs
            .write_all(&(crate::frame::MAX_FRAME as u32).to_be_bytes())
            .await
            .unwrap();
        let r = tokio::time::timeout(Duration::from_secs(1), accept(&mut ours, &me)).await;
        assert!(matches!(r, Ok(Err(NetError::FrameTooLarge))));
    }
}
