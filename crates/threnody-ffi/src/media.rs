//! Media at rest (see `threnody_core::media_seal`): the apps keep received
//! photos, voice and video messages and private files sealed under a key
//! derived from the identity, so a copy of their files is useless without
//! the identity's protection (passphrase, keyring or keystore).
//!
//! Apps seal with the main identity's node, personas' files included:
//! burning a persona deletes its folder, and its files are sealed besides.
//! Files kept before media was sealed are read as they are until
//! [`ThrenodyNode::seal_media_dir`] seals them.

use std::path::Path;

use threnody_core::media_seal::{self, MediaKey};

use crate::{Result, ThrenodyNode, fail};

impl ThrenodyNode {
    fn media_key(&self) -> MediaKey {
        MediaKey::new(self.node.identity_ref())
    }
}

#[uniffi::export]
impl ThrenodyNode {
    /// Seals `data` into a new file at `dst` (owner-only; replaces one
    /// there, atomically).
    pub fn seal_media(&self, data: Vec<u8>, dst: String) -> Result<()> {
        self.media_key()
            .seal_to_file(&data, Path::new(&dst))
            .map_err(fail)
    }

    /// Seals the file `src` into `dst`, a chunk at a time. With `src` the
    /// same as `dst` it is sealed in place and the plaintext overwritten
    /// first; otherwise `src` stays (delete it with `secure_delete_file`).
    pub fn seal_media_file(&self, src: String, dst: String) -> Result<()> {
        self.media_key()
            .seal_file(Path::new(&src), Path::new(&dst))
            .map_err(fail)
    }

    /// Decrypts the file `src` into `dst` (owner-only, created or
    /// replaced), for a player or another app that needs a file. A file
    /// kept before media was sealed is copied. Put `dst` in a cache folder
    /// that isn't backed up and delete it when done.
    pub fn open_media_file(&self, src: String, dst: String) -> Result<()> {
        self.media_key()
            .open_file(Path::new(&src), Path::new(&dst))
            .map_err(fail)
    }

    /// The plaintext of the file at `path`, decrypted in memory (a file
    /// kept before media was sealed: as it is).
    pub fn open_media(&self, path: String) -> Result<Vec<u8>> {
        self.media_key().read_file(Path::new(&path)).map_err(fail)
    }

    /// Seals, in place, every file directly in `dir` kept before media was
    /// sealed (overwriting the plaintext); how many. Run at startup.
    pub fn seal_media_dir(&self, dir: String) -> Result<u32> {
        self.media_key().seal_dir(Path::new(&dir)).map_err(fail)
    }
}

/// How many bytes the file at `path` holds once opened: from a sealed
/// file's length (not checked until it is opened), or a plain file's.
#[uniffi::export]
pub fn media_size(path: String) -> Result<u64> {
    let p = Path::new(&path);
    let len = std::fs::metadata(p).map_err(fail)?.len();
    if media_seal::is_sealed_file(p).map_err(fail)? {
        media_seal::plaintext_len(len).ok_or_else(|| fail("damaged media file"))
    } else {
        Ok(len)
    }
}

/// Whether the file at `path` is sealed media (false if it can't be read).
#[uniffi::export]
pub fn is_sealed_media(path: String) -> bool {
    media_seal::is_sealed_file(Path::new(&path)).unwrap_or(false)
}

/// Overwrites the file at `path`, then deletes it (best effort on flash
/// storage); no error if it is gone. For decrypted copies and recordings
/// once sealed.
#[uniffi::export]
pub fn secure_delete_file(path: String) -> Result<()> {
    threnody_core::store::secure_delete(Path::new(&path)).map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_seals_under_the_node_identity() {
        let dir = tempfile::tempdir().unwrap();
        let open = |n: &str| {
            ThrenodyNode::open(dir.path().join(n).display().to_string(), None, None).unwrap()
        };
        let (a, b) = (open("a"), open("b"));
        let s = |p: &str| dir.path().join(p).display().to_string();

        a.seal_media(b"photo".to_vec(), s("p.jpg")).unwrap();
        assert!(is_sealed_media(s("p.jpg")));
        assert!(
            !std::fs::read(s("p.jpg"))
                .unwrap()
                .windows(5)
                .any(|w| w == b"photo")
        );
        assert_eq!(media_size(s("p.jpg")).unwrap(), 5);
        assert_eq!(a.open_media(s("p.jpg")).unwrap(), b"photo");
        assert!(b.open_media(s("p.jpg")).is_err());

        a.open_media_file(s("p.jpg"), s("tmp.jpg")).unwrap();
        assert_eq!(std::fs::read(s("tmp.jpg")).unwrap(), b"photo");
        secure_delete_file(s("tmp.jpg")).unwrap();
        assert!(!Path::new(&s("tmp.jpg")).exists());

        // Recorded plainly, then sealed in place.
        std::fs::write(s("v.m4a"), b"voice").unwrap();
        assert_eq!(media_size(s("v.m4a")).unwrap(), 5);
        assert_eq!(a.open_media(s("v.m4a")).unwrap(), b"voice");
        a.seal_media_file(s("v.m4a"), s("v.m4a")).unwrap();
        assert!(is_sealed_media(s("v.m4a")));
        assert_eq!(a.open_media(s("v.m4a")).unwrap(), b"voice");

        // Kept before sealing, sealed at startup.
        let media = dir.path().join("media");
        std::fs::create_dir(&media).unwrap();
        std::fs::write(media.join("old.jpg"), b"old").unwrap();
        assert_eq!(a.seal_media_dir(media.display().to_string()).unwrap(), 1);
        assert_eq!(
            a.open_media(media.join("old.jpg").display().to_string())
                .unwrap(),
            b"old"
        );
    }
}
