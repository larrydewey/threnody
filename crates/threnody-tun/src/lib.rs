//! A TUN interface from a file descriptor, such as the one Android's
//! `VpnService` gives, for Threnody's userspace WireGuard
//! (`threnody-net::wg_userspace`).
//!
//! This is the workspace's only crate that allows `unsafe`, kept to the
//! system calls that need it.

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

    use tokio::io::unix::AsyncFd;

    /// An open TUN interface: whole IP packets in and out.
    pub struct FdTun(AsyncFd<OwnedFd>);

    impl FdTun {
        /// Takes over `fd`, an open TUN descriptor the platform handed to
        /// this process (on Android, `ParcelFileDescriptor.detachFd()`),
        /// and closes it when dropped. Nothing else may use or close it.
        /// Negative values are refused.
        pub fn adopt(fd: RawFd) -> io::Result<Self> {
            if fd < 0 {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            // SAFETY: by this function's contract the caller owns `fd` and
            // gives it up, so nothing else closes it.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            // SAFETY: fcntl reads and sets flags on a descriptor we own.
            let ok = unsafe {
                let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
                flags >= 0
                    && libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0
            };
            if !ok {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(AsyncFd::new(fd)?))
        }

        /// Reads one packet into `buf`; returns its length.
        pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            loop {
                let mut guard = self.0.readable().await?;
                let r = guard.try_io(|fd| {
                    // SAFETY: reads at most `buf.len()` bytes into `buf`,
                    // which we borrow mutably for the call.
                    let n =
                        unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                    usize::try_from(n).map_err(|_| io::Error::last_os_error())
                });
                if let Ok(r) = r {
                    return r;
                }
            }
        }

        /// Writes one packet.
        pub async fn write(&self, packet: &[u8]) -> io::Result<()> {
            loop {
                let mut guard = self.0.writable().await?;
                let r = guard.try_io(|fd| {
                    // SAFETY: writes `packet.len()` bytes from `packet`,
                    // which we borrow for the call.
                    let n = unsafe {
                        libc::write(fd.as_raw_fd(), packet.as_ptr().cast(), packet.len())
                    };
                    usize::try_from(n)
                        .map(drop)
                        .map_err(|_| io::Error::last_os_error())
                });
                if let Ok(r) = r {
                    return r;
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::fd::IntoRawFd;

        #[tokio::test]
        async fn packets_pass_through_a_descriptor() {
            // A socket pair stands in for a TUN device: datagrams keep
            // packet boundaries, as TUN reads and writes do.
            let (a, b) = std::os::unix::net::UnixDatagram::pair().unwrap();
            let tun = FdTun::adopt(a.into_raw_fd()).unwrap();
            b.send(b"packet one").unwrap();
            let mut buf = [0u8; 64];
            let n = tun.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"packet one");
            tun.write(b"reply").await.unwrap();
            let n = b.recv(&mut buf).unwrap();
            assert_eq!(&buf[..n], b"reply");
            assert!(FdTun::adopt(-1).is_err());
        }
    }
}

#[cfg(unix)]
pub use unix::FdTun;
