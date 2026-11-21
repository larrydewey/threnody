//! Files too large for one message (Appendix U): the app's store for them,
//! sending by location, and downloads of offered files.
//!
//! An app opts in with [`ThrenodyNode::set_large_file_store`], which also
//! tells contacts this device takes offers. Without it, nothing changes:
//! files go inline, at most `max_file_size` bytes.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use threnody_core::history::ConversationId;
use threnody_core::media_seal::MediaKey;
use threnody_net::SendOptions;

use crate::{FileOptions, Result, ThrenodyNode, fail};

/// A file the app set aside for a download: where the node writes it
/// (sealed), and the location history records for it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ReservedFile {
    pub path: String,
    pub location: String,
}

/// Where the app keeps files too large for one message. Called from a
/// Rust worker thread.
#[uniffi::export(with_foreign)]
pub trait LargeFileStore: Send + Sync {
    /// How many bytes the file at `location` holds as read (a sealed file
    /// opened); null if it is gone.
    fn size(&self, location: String) -> Option<u64>;
    /// Up to `len` bytes of the file at `location` from `offset`, fewer
    /// only at its end; null if it can't be read. Sealed files: use
    /// `read_media_range`.
    fn read_at(&self, location: String, offset: u64, len: u32) -> Option<Vec<u8>>;
    /// A new, empty private file for a received file called `name` (a
    /// voice or video message if `clip`), as `keep` would make; null if
    /// none can be made. The node seals the file into `path`.
    fn reserve(&self, name: String, clip: bool) -> Option<ReservedFile>;
    /// Bytes free where received files are kept; null if unknown.
    fn free_space(&self) -> Option<u64>;
}

/// Where an offered file's download is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TransferState {
    /// Not asked for: show its size and a download button.
    Idle,
    /// Asked for; waiting for the sender to be reachable.
    Waiting,
    Fetching {
        received: u64,
        total: u64,
    },
    /// Too little space left: say so, and offer to try again.
    NoSpace,
    /// The sender no longer has it.
    Unavailable,
    /// It didn't arrive intact, or couldn't be kept: try again.
    Failed,
    /// Here now: reload the conversation.
    Done,
}

impl From<threnody_net::TransferState> for TransferState {
    fn from(s: threnody_net::TransferState) -> Self {
        use threnody_net::TransferState as S;
        match s {
            S::Idle => Self::Idle,
            S::Waiting => Self::Waiting,
            S::Fetching { received, total } => Self::Fetching { received, total },
            S::NoSpace => Self::NoSpace,
            S::Unavailable => Self::Unavailable,
            S::Failed => Self::Failed,
            S::Done => Self::Done,
        }
    }
}

/// The app's [`LargeFileStore`] as the node's, sealing received files
/// under `key` as the app seals received media.
struct AppLarge {
    app: Arc<dyn LargeFileStore>,
    key: MediaKey,
}

impl threnody_net::LargeFileStore for AppLarge {
    fn size(&self, location: &str) -> Option<u64> {
        self.app.size(location.to_owned())
    }

    fn read_at(&self, location: &str, offset: u64, len: usize) -> Option<Vec<u8>> {
        self.app
            .read_at(location.to_owned(), offset, u32::try_from(len).ok()?)
    }

    fn keep_stream(&self, name: &str, clip: bool, src: &mut dyn Read) -> Option<String> {
        let r = self.app.reserve(name.to_owned(), clip)?;
        let path = Path::new(&r.path);
        match self.key.seal_reader_to_file(src, path) {
            Ok(_) => Some(r.location),
            Err(_) => {
                let _ = std::fs::remove_file(path);
                None
            }
        }
    }

    fn free_space(&self) -> Option<u64> {
        self.app.free_space()
    }
}

impl ThrenodyNode {
    fn conv_of(&self, peer: &str) -> Result<ConversationId> {
        let p = self.resolve(peer)?;
        Ok(self.node.conversation_for(&p))
    }
}

#[uniffi::export]
impl ThrenodyNode {
    /// Where this device keeps files too large for one message, so it can
    /// send them by location and download offered ones. Received files
    /// are sealed under this node's identity, as `seal_media` seals them:
    /// set it on the main identity's node, at start, before `start`.
    /// Contacts learn this device takes offers from their next session.
    pub fn set_large_file_store(&self, store: Arc<dyn LargeFileStore>) {
        let _guard = self.rt.enter();
        let key = MediaKey::new(self.node.identity_ref());
        self.node
            .set_large_file_store(Arc::new(AppLarge { app: store, key }));
    }

    /// Sends the file at `location` (read through the large-file store) to
    /// `peer`, recording it with that location. A file that fits a message
    /// goes inline, as `send_file` sends it; a larger one is offered, if
    /// the peer takes offers, and downloaded by it from this device. Reads
    /// the whole file: call off the main thread.
    pub fn send_file_at(
        &self,
        peer: String,
        name: String,
        location: String,
        options: FileOptions,
    ) -> Result<()> {
        let p = self.resolve(&peer)?;
        self.try_reach(&p);
        let _guard = self.rt.enter();
        self.node
            .send_file_at(
                &p,
                name,
                location,
                SendOptions {
                    sensitive: options.sensitive,
                    caption: options.caption,
                    album: options.album,
                    clip: options.clip.map(Into::into),
                    forwarded: options.forwarded,
                },
            )
            .map_err(fail)
    }

    /// Where the download of the file `peer` offered as message `id` is;
    /// null if it isn't an offer still waiting (here already, or never
    /// offered).
    pub fn transfer_state(&self, peer: String, id: u64) -> Option<TransferState> {
        let conv = self.conv_of(&peer).ok()?;
        self.node.transfer_state(conv, id).map(Into::into)
    }

    /// Downloads the file `peer` offered as message `id` (again, after
    /// it ran out of space or failed).
    pub fn download_file(&self, peer: String, id: u64) -> Result<TransferState> {
        let conv = self.conv_of(&peer)?;
        let _guard = self.rt.enter();
        self.node.download(conv, id).map(Into::into).map_err(fail)
    }

    /// Stops downloading the file `peer` offered as message `id`,
    /// deleting what came.
    pub fn cancel_download(&self, peer: String, id: u64) -> Result<()> {
        let conv = self.conv_of(&peer)?;
        let _guard = self.rt.enter();
        self.node.cancel_download(conv, id);
        Ok(())
    }

    /// Up to `len` bytes of the media file at `path` from `offset`,
    /// decrypting only the chunks they lie in (a file kept before media
    /// was sealed: read as it is). For a large-file store's `read_at`, and
    /// for players that read a file in pieces.
    pub fn read_media_range(&self, path: String, offset: u64, len: u32) -> Result<Vec<u8>> {
        MediaKey::new(self.node.identity_ref())
            .read_range(Path::new(&path), offset, len as usize)
            .map_err(fail)
    }
}
