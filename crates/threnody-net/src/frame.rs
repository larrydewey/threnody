//! Stream framing: `u32 big-endian length || frame`. Used by every
//! stream-oriented transport (TCP today; Bluetooth L2CAP / Wi-Fi Direct
//! sockets later).

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{NetError, Result};

/// Largest accepted frame: one maximal core message plus envelope headroom.
pub const MAX_FRAME: usize = threnody_core::cbor::MAX_ENCODED + 4096;

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
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(NetError::FrameTooLarge);
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
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
}
