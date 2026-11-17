//! Channels that hold at most a byte budget. What a peer sends us waits
//! in one of these until a local task takes it; a peer that sends faster
//! than that task drains (it never reads our side, say) fills the budget,
//! and the sender is told to give up on the circuit rather than keep
//! everything in memory.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::mpsc;

/// Creates a channel holding at most `cap` bytes, as the sender counts
/// them. One item always fits into an empty channel, however large.
pub(crate) fn channel<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let held = Arc::new(AtomicUsize::new(0));
    (
        Sender {
            tx,
            held: held.clone(),
            cap,
        },
        Receiver { rx, held },
    )
}

pub(crate) struct Sender<T> {
    tx: mpsc::UnboundedSender<(usize, T)>,
    held: Arc<AtomicUsize>,
    cap: usize,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            held: self.held.clone(),
            cap: self.cap,
        }
    }
}

impl<T> Sender<T> {
    /// Queues `item`, `len` bytes of it. False when the receiver is gone,
    /// or the budget has no room for it: then the caller should end what
    /// feeds the channel.
    pub(crate) fn send(&self, len: usize, item: T) -> bool {
        let fits = self
            .held
            .try_update(Ordering::AcqRel, Ordering::Acquire, |h| {
                (h == 0 || h.saturating_add(len) <= self.cap).then(|| h.saturating_add(len))
            })
            .is_ok();
        if !fits {
            return false;
        }
        if self.tx.send((len, item)).is_err() {
            self.held.fetch_sub(len, Ordering::AcqRel);
            return false;
        }
        true
    }
}

pub(crate) struct Receiver<T> {
    rx: mpsc::UnboundedReceiver<(usize, T)>,
    held: Arc<AtomicUsize>,
}

impl<T> Receiver<T> {
    /// The next item; `None` once every sender is gone. Cancel-safe, as
    /// `mpsc::UnboundedReceiver::recv` is.
    pub(crate) async fn recv(&mut self) -> Option<T> {
        let (len, item) = self.rx.recv().await?;
        self.held.fetch_sub(len, Ordering::AcqRel);
        Some(item)
    }

    /// The next item if one is waiting.
    #[cfg(test)]
    pub(crate) fn try_recv(&mut self) -> Option<T> {
        let (len, item) = self.rx.try_recv().ok()?;
        self.held.fetch_sub(len, Ordering::AcqRel);
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn holds_up_to_its_budget() {
        let (tx, mut rx) = channel::<Vec<u8>>(10);
        // One item always fits an empty channel.
        assert!(tx.send(25, vec![0; 25]));
        assert!(!tx.send(1, vec![1]));
        assert_eq!(rx.recv().await.unwrap().len(), 25);
        assert!(tx.send(6, vec![0; 6]));
        assert!(tx.send(4, vec![0; 4]));
        assert!(!tx.send(1, vec![0]), "full");
        rx.recv().await.unwrap();
        assert!(tx.send(5, vec![0; 5]));
        drop(rx);
        assert!(!tx.send(0, vec![]), "receiver gone");
    }
}
