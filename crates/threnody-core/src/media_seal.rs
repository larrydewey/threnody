//! Media at rest (Appendix C, *Local storage*): received photos, voice and
//! video messages and files the apps keep privately are sealed under a key
//! derived from the identity seed, so they are protected exactly as the
//! identity is (passphrase, keyring or platform keystore). A copy of the
//! app's files is useless without it.
//!
//! ```text
//! sealed  = MAGIC || salt (24) || chunk_0 || ... || chunk_n
//! chunk_i = XChaCha20-Poly1305(file_key, nonce_i, plaintext_i, aad = MAGIC || salt)
//! file_key = KDF(MEDIA_FILE_KEY, media_key, salt)
//! media_key = KDF(MEDIA_KEY, identity seed)
//! nonce_i = 0^15 || u64be(i) || last
//! ```
//!
//! Every chunk but the last carries exactly [`CHUNK`] bytes of plaintext;
//! the last carries 0..=[`CHUNK`] and has `last = 1`, so an empty file is
//! one empty final chunk. This is the STREAM construction (Hoang, Reyhanitabar,
//! Rogaway, Vizár): reordered, dropped, truncated or appended chunks fail
//! to open. Files are read and written a chunk at a time, so a long video
//! is never held in memory whole.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use zeroize::Zeroizing;

use crate::crypto::{kdf, random_bytes};
use crate::error::{Error, Result};
use crate::identity::Identity;

/// What every sealed media file starts with.
pub const MAGIC: &[u8; 10] = b"thr-media1";
/// Random per-file salt after the magic.
pub const SALT_LEN: usize = 24;
/// Header length: magic and salt.
pub const HEADER_LEN: usize = MAGIC.len() + SALT_LEN;
/// Plaintext bytes per chunk.
pub const CHUNK: usize = 64 * 1024;
/// Poly1305 tag per chunk.
pub const TAG_LEN: usize = 16;
const SEALED_CHUNK: usize = CHUNK + TAG_LEN;

/// The key media is sealed under, derived from an identity.
pub struct MediaKey(Zeroizing<[u8; 32]>);

/// Whether `bytes` (at least the first [`MAGIC`]`.len()` of a file) start
/// like sealed media. Files without it were kept before media was sealed.
pub fn is_sealed(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

/// Whether the file at `path` is sealed media.
pub fn is_sealed_file(path: &Path) -> Result<bool> {
    let mut f = fs::File::open(path)?;
    let mut head = [0u8; MAGIC.len()];
    let n = read_full(&mut f, &mut head)?;
    Ok(is_sealed(&head[..n]))
}

/// The plaintext length of a sealed file `sealed_len` bytes long, from its
/// length alone (not authenticated: only for display and size limits).
pub fn plaintext_len(sealed_len: u64) -> Option<u64> {
    let body = sealed_len.checked_sub(HEADER_LEN as u64)?;
    let (full, rem) = (body / SEALED_CHUNK as u64, body % SEALED_CHUNK as u64);
    match rem {
        // Ends with a full final chunk.
        0 if full > 0 => Some(full * CHUNK as u64),
        r if r >= TAG_LEN as u64 => Some(full * CHUNK as u64 + r - TAG_LEN as u64),
        _ => None,
    }
}

struct Cipher {
    aead: XChaCha20Poly1305,
    header: [u8; HEADER_LEN],
    counter: u64,
    done: bool,
}

impl Cipher {
    fn new(key: &MediaKey, header: [u8; HEADER_LEN]) -> Self {
        let salt = &header[MAGIC.len()..];
        let file_key: Zeroizing<[u8; 32]> =
            Zeroizing::new(kdf::derive(kdf::label::MEDIA_FILE_KEY, &[&key.0[..], salt]));
        Self {
            aead: XChaCha20Poly1305::new((&*file_key).into()),
            header,
            counter: 0,
            done: false,
        }
    }

    fn nonce(&self, last: bool) -> [u8; 24] {
        let mut n = [0u8; 24];
        n[15..23].copy_from_slice(&self.counter.to_be_bytes());
        n[23] = u8::from(last);
        n
    }

    fn seal(&mut self, pt: &[u8], last: bool) -> Result<Vec<u8>> {
        if self.done {
            return Err(Error::Malformed("sealed media: chunk after the last"));
        }
        let nonce = self.nonce(last);
        let ct = self
            .aead
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: pt,
                    aad: &self.header,
                },
            )
            .map_err(|_| Error::Malformed("sealed media: chunk too large"))?;
        self.step(last)?;
        Ok(ct)
    }

    fn open(&mut self, ct: &[u8], last: bool) -> Result<Vec<u8>> {
        if self.done {
            return Err(Error::Decrypt);
        }
        let nonce = self.nonce(last);
        let pt = self
            .aead
            .decrypt(
                (&nonce).into(),
                Payload {
                    msg: ct,
                    aad: &self.header,
                },
            )
            .map_err(|_| Error::Decrypt)?;
        self.step(last)?;
        Ok(pt)
    }

    fn step(&mut self, last: bool) -> Result<()> {
        self.done = last;
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or(Error::Malformed("sealed media: too many chunks"))?;
        Ok(())
    }
}

/// Reads until `buf` is full or the input ends; how many bytes it got.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Splits input into blocks of `size`, telling which one is the last
/// (looking one block ahead, so a file of exactly `k * size` bytes ends
/// with a full last block, and an empty one is a single empty block).
struct Blocks<R> {
    r: R,
    cur: Vec<u8>,
    next: Vec<u8>,
    started: bool,
    finished: bool,
}

impl<R: Read> Blocks<R> {
    fn new(r: R, size: usize) -> Self {
        Self {
            r,
            cur: vec![0u8; size],
            next: vec![0u8; size],
            started: false,
            finished: false,
        }
    }

    /// The next block and whether it is the last; `None` after the last.
    fn next_block(&mut self) -> std::io::Result<Option<(&[u8], bool)>> {
        if self.finished {
            return Ok(None);
        }
        let size = self.cur.len();
        if !self.started {
            self.started = true;
            let n = read_full(&mut self.r, &mut self.next)?;
            self.next.truncate(n);
        }
        std::mem::swap(&mut self.cur, &mut self.next);
        // `cur` now holds the block to return; read the one after it.
        self.next.resize(size.max(self.cur.len()), 0);
        let full = self.cur.len() == size;
        let n = if full {
            read_full(&mut self.r, &mut self.next)?
        } else {
            0
        };
        self.next.truncate(n);
        let last = n == 0;
        if last {
            self.finished = true;
        }
        Ok(Some((&self.cur, last)))
    }
}

impl<R> Drop for Blocks<R> {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.cur.zeroize();
        self.next.zeroize();
    }
}

impl MediaKey {
    /// The media key of `identity`.
    pub fn new(identity: &Identity) -> Self {
        let seed = identity.seed();
        Self(Zeroizing::new(kdf::derive(
            kdf::label::MEDIA_KEY,
            &[&seed[..]],
        )))
    }

    /// A key from raw bytes (tests, and callers that derived it already).
    pub fn from_bytes(key: [u8; 32]) -> Self {
        Self(Zeroizing::new(key))
    }

    /// Seals everything `r` reads into `w`; the plaintext length.
    pub fn seal_stream(&self, r: impl Read, w: &mut impl Write) -> Result<u64> {
        let mut header = [0u8; HEADER_LEN];
        header[..MAGIC.len()].copy_from_slice(MAGIC);
        header[MAGIC.len()..].copy_from_slice(&random_bytes::<SALT_LEN>());
        w.write_all(&header)?;
        let mut c = Cipher::new(self, header);
        let mut blocks = Blocks::new(r, CHUNK);
        let mut total = 0u64;
        while let Some((pt, last)) = blocks.next_block()? {
            total += pt.len() as u64;
            w.write_all(&c.seal(pt, last)?)?;
        }
        Ok(total)
    }

    /// Opens sealed media `r` reads into `w`; the plaintext length. On
    /// failure `w` may hold a verified prefix of the plaintext: callers
    /// writing to a file remove it.
    pub fn open_stream(&self, mut r: impl Read, w: &mut impl Write) -> Result<u64> {
        let mut header = [0u8; HEADER_LEN];
        if read_full(&mut r, &mut header)? != HEADER_LEN || !is_sealed(&header) {
            return Err(Error::Malformed("not sealed media"));
        }
        let mut c = Cipher::new(self, header);
        let mut blocks = Blocks::new(r, SEALED_CHUNK);
        let mut total = 0u64;
        while let Some((ct, last)) = blocks.next_block()? {
            let pt = Zeroizing::new(c.open(ct, last)?);
            total += pt.len() as u64;
            w.write_all(&pt)?;
        }
        if !c.done {
            return Err(Error::Decrypt);
        }
        Ok(total)
    }

    /// Seals `pt` in memory.
    pub fn seal(&self, pt: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(sealed_len(pt.len() as u64) as usize);
        // Neither a slice nor a Vec fails to read or write.
        self.seal_stream(pt, &mut out)
            .expect("sealing in memory cannot fail");
        out
    }

    /// Opens sealed media held in memory.
    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(plaintext_len(sealed.len() as u64).unwrap_or(0) as usize);
        self.open_stream(sealed, &mut out)?;
        Ok(out)
    }

    /// Reads the file at `path`: opened if sealed, as it is if kept before
    /// media was sealed.
    pub fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        let raw = fs::read(path)?;
        if is_sealed(&raw) {
            self.open(&raw)
        } else {
            Ok(raw)
        }
    }

    /// Seals `data` into a new file at `dst` (replacing one there).
    pub fn seal_to_file(&self, data: &[u8], dst: &Path) -> Result<()> {
        replace_with(dst, |w| self.seal_stream(data, w).map(|_| ()))
    }

    /// Seals everything `r` reads into a new file at `dst` (replacing one
    /// there), a chunk at a time; the plaintext length.
    pub fn seal_reader_to_file(&self, r: impl Read, dst: &Path) -> Result<u64> {
        let mut total = 0;
        replace_with(dst, |w| {
            total = self.seal_stream(r, w)?;
            Ok(())
        })?;
        Ok(total)
    }

    /// Up to `len` bytes of what the file at `path` holds from `offset`
    /// (fewer at its end, none past it): decrypting only the chunks they
    /// lie in if it is sealed, read as they are if it was kept before
    /// media was sealed.
    pub fn read_range(&self, path: &Path, offset: u64, len: usize) -> Result<Vec<u8>> {
        use std::io::{Seek, SeekFrom};
        let mut f = fs::File::open(path)?;
        let file_len = f.metadata()?.len();
        let mut header = [0u8; HEADER_LEN];
        let n = read_full(&mut f, &mut header)?;
        if !is_sealed(&header[..n.min(MAGIC.len())]) || n < HEADER_LEN {
            let end = offset.saturating_add(len as u64).min(file_len);
            if offset >= end {
                return Ok(Vec::new());
            }
            f.seek(SeekFrom::Start(offset))?;
            let mut out = vec![0u8; (end - offset) as usize];
            f.read_exact(&mut out)?;
            return Ok(out);
        }
        let total = plaintext_len(file_len).ok_or(Error::Malformed("damaged media file"))?;
        let end = offset.saturating_add(len as u64).min(total);
        if offset >= end {
            return Ok(Vec::new());
        }
        let body = file_len - HEADER_LEN as u64;
        let chunks = body.div_ceil(SEALED_CHUNK as u64);
        let first = offset / CHUNK as u64;
        let last = (end - 1) / CHUNK as u64;
        let mut c = Cipher::new(self, header);
        c.counter = first;
        f.seek(SeekFrom::Start(
            HEADER_LEN as u64 + first * SEALED_CHUNK as u64,
        ))?;
        let mut out = Vec::with_capacity((end - offset) as usize);
        let mut ct = vec![0u8; SEALED_CHUNK];
        for i in first..=last {
            let want = if i + 1 == chunks {
                (body - i * SEALED_CHUNK as u64) as usize
            } else {
                SEALED_CHUNK
            };
            f.read_exact(&mut ct[..want])?;
            let pt = Zeroizing::new(c.open(&ct[..want], i + 1 == chunks)?);
            let from = if i == first {
                (offset - first * CHUNK as u64) as usize
            } else {
                0
            };
            let to = if i == last {
                (end - last * CHUNK as u64) as usize
            } else {
                pt.len()
            };
            out.extend_from_slice(pt.get(from..to).ok_or(Error::Decrypt)?);
        }
        Ok(out)
    }

    /// Seals the file `src` into `dst`, a chunk at a time. With `src` and
    /// `dst` the same file, seals it in place: the plaintext is overwritten
    /// ([`crate::store::secure_delete`]) before the sealed copy takes its
    /// name. Otherwise `src` is left as it is.
    pub fn seal_file(&self, src: &Path, dst: &Path) -> Result<()> {
        let tmp = sealing_path(dst);
        let res = (|| {
            let input = fs::File::open(src)?;
            write_new(&tmp, |w| {
                self.seal_stream(std::io::BufReader::new(input), w)
                    .map(|_| ())
            })
        })();
        if let Err(e) = res {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        if same_file(src, dst) {
            crate::store::secure_delete(src)?;
        }
        fs::rename(&tmp, dst)?;
        Ok(())
    }

    /// Opens the file `src` into `dst` (created or replaced, owner-only):
    /// decrypted if sealed, copied as it is if kept before media was
    /// sealed. A file that fails to open leaves no `dst`.
    pub fn open_file(&self, src: &Path, dst: &Path) -> Result<()> {
        let mut input = std::io::BufReader::new(fs::File::open(src)?);
        let sealed = {
            use std::io::BufRead;
            is_sealed(input.fill_buf()?)
        };
        let res = (|| {
            let mut out = std::io::BufWriter::new(private_file(dst, true)?);
            if sealed {
                self.open_stream(input, &mut out)?;
            } else {
                std::io::copy(&mut input, &mut out)?;
            }
            out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
            Ok(())
        })();
        if res.is_err() {
            let _ = fs::remove_file(dst);
        }
        res
    }

    /// Seals every plain file directly in `dir` in place (media kept before
    /// sealing); how many. Finishes in-place seals a crash interrupted.
    /// Files that can't be sealed are left for the next run.
    pub fn seal_dir(&self, dir: &Path) -> Result<u32> {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut sealed = 0;
        let mut files = Vec::new();
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_file()) {
                files.push(e.path());
            }
        }
        for path in &files {
            if let Some(target) = sealing_target(path) {
                // An interrupted in-place seal: done but for the rename
                // (the plaintext is gone), or not done (it is still there).
                if target.exists() {
                    let _ = fs::remove_file(path);
                } else if is_sealed_file(path).unwrap_or(false) {
                    let _ = fs::rename(path, &target);
                }
            }
        }
        for path in files.iter().filter(|p| sealing_target(p).is_none()) {
            // Empty files are placeholders an app reserved a name with.
            let plain = fs::metadata(path).is_ok_and(|m| m.len() > 0)
                && !is_sealed_file(path).unwrap_or(true);
            if plain && self.seal_file(path, path).is_ok() {
                sealed += 1;
            }
        }
        Ok(sealed)
    }
}

/// The length of `pt_len` bytes once sealed.
pub fn sealed_len(pt_len: u64) -> u64 {
    let chunks = pt_len.div_ceil(CHUNK as u64).max(1);
    HEADER_LEN as u64 + pt_len + chunks * TAG_LEN as u64
}

const SEALING: &str = ".sealing";

fn sealing_path(dst: &Path) -> PathBuf {
    let mut name = dst.file_name().unwrap_or_default().to_os_string();
    name.push(SEALING);
    dst.with_file_name(name)
}

fn sealing_target(p: &Path) -> Option<PathBuf> {
    let name = p.file_name()?.to_str()?;
    let base = name.strip_suffix(SEALING)?;
    (!base.is_empty()).then(|| p.with_file_name(base))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Opens `path` for writing, readable by the owner only: a new file, or
/// (with `replace`) truncating one there.
fn private_file(path: &Path, replace: bool) -> std::io::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true);
    if replace {
        opts.create(true).truncate(true);
    } else {
        opts.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Writes a new file at `path` (replacing a stale one) with `fill`, synced.
fn write_new(
    path: &Path,
    fill: impl FnOnce(&mut std::io::BufWriter<fs::File>) -> Result<()>,
) -> Result<()> {
    let mut out = std::io::BufWriter::new(private_file(path, true)?);
    fill(&mut out)?;
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok(())
}

/// Atomically replaces `dst` with what `fill` writes.
fn replace_with(
    dst: &Path,
    fill: impl FnOnce(&mut std::io::BufWriter<fs::File>) -> Result<()>,
) -> Result<()> {
    let tmp = sealing_path(dst);
    if let Err(e) = write_new(&tmp, fill) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, dst)?;
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn ranges_read_only_what_they_ask() {
        let dir = tempfile::tempdir().unwrap();
        let key = MediaKey::from_bytes([3; 32]);
        for len in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 3 * CHUNK + 17] {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 % 256) as u8).collect();
            let sealed = dir.path().join(format!("s{len}"));
            assert_eq!(
                key.seal_reader_to_file(&data[..], &sealed).unwrap(),
                len as u64
            );
            let plain = dir.path().join(format!("p{len}"));
            fs::write(&plain, &data).unwrap();
            for (off, n) in [
                (0, len),
                (0, 10),
                (5, CHUNK),
                (CHUNK.saturating_sub(3), 7),
                (len.saturating_sub(1), 100),
                (len, 5),
                (len + 10, 5),
            ] {
                let start = off.min(len);
                let end = (off + n).min(len);
                let want = &data[start..end.max(start)];
                for p in [&sealed, &plain] {
                    assert_eq!(
                        key.read_range(p, off as u64, n).unwrap(),
                        want,
                        "len {len} at {off}+{n}"
                    );
                }
            }
        }
        // A sealed file cut at a chunk boundary doesn't read as ending there.
        let data = vec![9u8; 2 * CHUNK + 5];
        let p = dir.path().join("cut");
        key.seal_reader_to_file(&data[..], &p).unwrap();
        let f = fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_len((HEADER_LEN + SEALED_CHUNK) as u64).unwrap();
        assert!(key.read_range(&p, 0, 10).is_err());
    }
    use super::*;

    fn key() -> MediaKey {
        MediaKey::from_bytes([9u8; 32])
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 + i / 7) as u8).collect()
    }

    #[test]
    fn round_trips_at_every_size_that_matters() {
        let k = key();
        for n in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK, 3 * CHUNK + 17] {
            let pt = data(n);
            let s = k.seal(&pt);
            assert!(is_sealed(&s));
            assert_eq!(s.len() as u64, sealed_len(n as u64), "{n}");
            assert_eq!(plaintext_len(s.len() as u64), Some(n as u64), "{n}");
            assert_eq!(k.open(&s).unwrap(), pt, "{n}");
        }
    }

    #[test]
    fn empty_file_is_one_empty_final_chunk() {
        let s = key().seal(b"");
        assert_eq!(s.len(), HEADER_LEN + TAG_LEN);
        assert!(key().open(&s).unwrap().is_empty());
        // Without its chunk it is truncated, not empty.
        assert!(key().open(&s[..HEADER_LEN]).is_err());
    }

    #[test]
    fn salts_differ() {
        let (a, b) = (key().seal(b"same"), key().seal(b"same"));
        assert_ne!(a, b);
    }

    #[test]
    fn wrong_key_fails() {
        let s = key().seal(&data(100));
        assert!(MediaKey::from_bytes([8u8; 32]).open(&s).is_err());
    }

    #[test]
    fn identity_keys_differ() {
        let (a, b) = (Identity::generate(), Identity::generate());
        let s = MediaKey::new(&a).seal(b"photo");
        assert_eq!(MediaKey::new(&a).open(&s).unwrap(), b"photo");
        assert!(MediaKey::new(&b).open(&s).is_err());
    }

    #[test]
    fn truncation_fails() {
        let k = key();
        let pt = data(2 * CHUNK + 5);
        let s = k.seal(&pt);
        // At a chunk boundary: the first two chunks are whole but neither
        // was sealed as the last.
        for cut in [
            HEADER_LEN + SEALED_CHUNK,
            HEADER_LEN + 2 * SEALED_CHUNK,
            s.len() - 1,
            HEADER_LEN + 3,
            5,
        ] {
            assert!(k.open(&s[..cut]).is_err(), "cut at {cut}");
        }
        // Exactly one full chunk: dropping the final full chunk.
        let s = k.seal(&data(2 * CHUNK));
        assert!(k.open(&s[..HEADER_LEN + SEALED_CHUNK]).is_err());
    }

    #[test]
    fn appended_data_fails() {
        let k = key();
        for n in [0, 10, CHUNK] {
            let mut s = k.seal(&data(n));
            s.push(0);
            assert!(k.open(&s).is_err(), "{n}");
        }
        // A whole extra chunk after the last.
        let mut s = k.seal(&data(CHUNK));
        let extra = s[HEADER_LEN..].to_vec();
        s.extend_from_slice(&extra);
        assert!(k.open(&s).is_err());
    }

    #[test]
    fn reordered_chunks_fail() {
        let k = key();
        let s = k.seal(&data(3 * CHUNK));
        let c = |i: usize| &s[HEADER_LEN + i * SEALED_CHUNK..HEADER_LEN + (i + 1) * SEALED_CHUNK];
        let mut swapped = s[..HEADER_LEN].to_vec();
        for i in [1, 0, 2] {
            swapped.extend_from_slice(c(i));
        }
        assert!(k.open(&swapped).is_err());
    }

    #[test]
    fn chunks_dont_move_between_files() {
        let k = key();
        let pt = data(2 * CHUNK);
        let (a, b) = (k.seal(&pt), k.seal(&pt));
        let mut mixed = a[..HEADER_LEN + SEALED_CHUNK].to_vec();
        mixed.extend_from_slice(&b[HEADER_LEN + SEALED_CHUNK..]);
        assert!(k.open(&mixed).is_err());
    }

    #[test]
    fn flipped_bits_fail() {
        let k = key();
        let s = k.seal(&data(1000));
        for i in [0, MAGIC.len(), HEADER_LEN, s.len() - 1] {
            let mut t = s.clone();
            t[i] ^= 1;
            assert!(k.open(&t).is_err(), "byte {i}");
        }
    }

    #[test]
    fn files_seal_open_and_migrate() {
        let dir = tempfile::tempdir().unwrap();
        let k = key();
        let pt = data(CHUNK * 2 + 3);
        let plain = dir.path().join("a.jpg");
        fs::write(&plain, &pt).unwrap();
        let sealed = dir.path().join("a.sealed");
        k.seal_file(&plain, &sealed).unwrap();
        assert!(plain.exists());
        assert!(is_sealed_file(&sealed).unwrap());
        let out = dir.path().join("out");
        k.open_file(&sealed, &out).unwrap();
        assert_eq!(fs::read(&out).unwrap(), pt);
        assert_eq!(k.read_file(&sealed).unwrap(), pt);
        // Kept before sealing: read as it is.
        assert_eq!(k.read_file(&plain).unwrap(), pt);
        k.open_file(&plain, &out).unwrap();
        assert_eq!(fs::read(&out).unwrap(), pt);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&sealed, &out] {
                assert_eq!(fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600);
            }
        }
        // A tampered file leaves nothing behind.
        let mut bad = fs::read(&sealed).unwrap();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        fs::write(&sealed, &bad).unwrap();
        fs::remove_file(&out).unwrap();
        assert!(k.open_file(&sealed, &out).is_err());
        assert!(!out.exists());

        // Migration: plain files sealed in place, placeholders and sealed
        // files left, an interrupted seal finished.
        let migrate = dir.path().join("media");
        fs::create_dir(&migrate).unwrap();
        fs::write(migrate.join("old.jpg"), b"old photo").unwrap();
        fs::write(migrate.join("reserved.jpg"), b"").unwrap();
        k.seal_to_file(b"new", &migrate.join("new.jpg")).unwrap();
        fs::write(migrate.join("crash.jpg.sealing"), k.seal(b"crashed")).unwrap();
        fs::write(migrate.join("half.jpg"), b"half").unwrap();
        fs::write(migrate.join("half.jpg.sealing"), b"partial").unwrap();
        assert_eq!(k.seal_dir(&migrate).unwrap(), 2);
        assert_eq!(k.read_file(&migrate.join("old.jpg")).unwrap(), b"old photo");
        assert!(is_sealed_file(&migrate.join("old.jpg")).unwrap());
        assert_eq!(fs::read(migrate.join("reserved.jpg")).unwrap(), b"");
        assert_eq!(k.read_file(&migrate.join("new.jpg")).unwrap(), b"new");
        assert_eq!(k.read_file(&migrate.join("crash.jpg")).unwrap(), b"crashed");
        assert_eq!(k.read_file(&migrate.join("half.jpg")).unwrap(), b"half");
        assert!(is_sealed_file(&migrate.join("half.jpg")).unwrap());
        let left: Vec<_> = fs::read_dir(&migrate)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(SEALING))
            .collect();
        assert!(left.is_empty());
        assert_eq!(k.seal_dir(&dir.path().join("none")).unwrap(), 0);
    }

    #[test]
    fn plaintext_len_rejects_impossible_lengths() {
        assert_eq!(plaintext_len(0), None);
        assert_eq!(plaintext_len(HEADER_LEN as u64), None);
        assert_eq!(plaintext_len(HEADER_LEN as u64 + 15), None);
        assert_eq!(plaintext_len(HEADER_LEN as u64 + 16), Some(0));
    }
}
