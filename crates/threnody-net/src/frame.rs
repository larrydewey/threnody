//! Stream framing: `u32 big-endian length || frame`. Used by every
//! stream-oriented transport (TCP today; Bluetooth L2CAP / Wi-Fi Direct
//! sockets later).

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{NetError, Result};

/// Largest accepted frame: one maximal core message plus envelope headroom.
pub const MAX_FRAME: usize = threnody_core::cbor::MAX_ENCODED + 4096;
/// Largest frame accepted before the peer is authenticated. Handshake
/// messages are under 3 KB, and nobody should get a stranger to hold
/// megabytes.
pub const MAX_HANDSHAKE_FRAME: usize = 16 * 1024;

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &[u8]) -> Result<()> {
    let len = u32::try_from(frame.len()).map_err(|_| NetError::FrameTooLarge)?;
    if frame.len() > MAX_FRAME {
        return Err(NetError::FrameTooLarge);
    }
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(frame).await?;
    w.flush().await?;
    Ok(())
}

/// Reads one frame; `Ok(None)` on clean end of stream.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>> {
    read_frame_max(r, MAX_FRAME).await
}

/// [`read_frame`] refusing anything longer than `max` before reading it.
pub async fn read_frame_max<R: AsyncRead + Unpin>(
    r: &mut R,
    max: usize,
) -> Result<Option<Vec<u8>>> {
    let Some(len) = read_len(r, max).await? else {
        return Ok(None);
    };
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

/// Reads a frame's length prefix, refusing anything over `max`; `Ok(None)`
/// on clean end of stream.
pub(crate) async fn read_len<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> Result<Option<usize>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > max {
        return Err(NetError::FrameTooLarge);
    }
    Ok(Some(len))
}

/// How much of a metered frame body is taken on at a time.
const BODY_STEP: usize = 64 * 1024;

/// Reads a frame body of `len` bytes for a peer we don't trust with
/// memory. The buffer grows a step at a time as bytes arrive, so a length
/// prefix alone costs at most one step, and `admit` is asked before each
/// step (false refuses the frame). A body that stops arriving for `stall`
/// ends the read.
pub(crate) async fn read_body_metered<R: AsyncRead + Unpin>(
    r: &mut R,
    len: usize,
    stall: Duration,
    mut admit: impl FnMut(usize) -> bool,
) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    while buf.len() < len {
        let step = (len - buf.len()).min(BODY_STEP);
        if !admit(step) {
            return Err(NetError::FrameTooLarge);
        }
        let mut filled = buf.len();
        buf.resize(filled + step, 0);
        while filled < buf.len() {
            let n = tokio::time::timeout(stall, r.read(&mut buf[filled..]))
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "frame stalled")
                })??;
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            filled += n;
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_reject_oversize() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        write_frame(&mut a, b"one").await.unwrap();
        write_frame(&mut a, b"").await.unwrap();
        assert_eq!(read_frame(&mut b).await.unwrap().unwrap(), b"one");
        assert_eq!(read_frame(&mut b).await.unwrap().unwrap(), b"");
        a.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        assert!(matches!(
            read_frame(&mut b).await,
            Err(NetError::FrameTooLarge)
        ));
        drop(a);
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn metered_bodies_grow_with_what_arrives_and_stall_out() {
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        // A full 16 MiB announced, 100 KB sent, then nothing.
        a.write_all(&vec![7; 100_000]).await.unwrap();
        let mut granted = 0;
        let stall = Duration::from_millis(200);
        let r = read_body_metered(&mut b, MAX_FRAME, stall, |n| {
            granted += n;
            true
        })
        .await;
        assert!(matches!(r, Err(NetError::Io(e)) if e.kind() == std::io::ErrorKind::TimedOut));
        // Two steps were taken on, not the whole announced length.
        assert_eq!(granted, 2 * BODY_STEP);

        // A complete body reads back whole; a refused step ends the read.
        a.write_all(&vec![9; BODY_STEP + 10]).await.unwrap();
        let body = read_body_metered(&mut b, BODY_STEP + 10, stall, |_| true)
            .await
            .unwrap();
        assert_eq!(body, vec![9; BODY_STEP + 10]);
        a.write_all(&[1; 10]).await.unwrap();
        let r = read_body_metered(&mut b, 10, stall, |_| false).await;
        assert!(matches!(r, Err(NetError::FrameTooLarge)));
    }
}
