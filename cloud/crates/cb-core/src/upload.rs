//! Storing uploads, with resume after an interrupted transfer.
//!
//! The stock `file_upload_handler` appends to a file on disk: a client whose
//! upload was cut off asks `<get_file_size>`, gets the number of bytes the
//! server holds, and sends the rest with `<offset>` set to that number
//! (`FILE_XFER::init_upload` in `client/file_xfer.cpp`). R2 cannot append,
//! so the bytes of an upload are kept as pieces:
//!
//! - The file is divided on a fixed grid of `piece` bytes. Every grid piece
//!   that ends before the last byte of the file is written as its own
//!   object, `partial/<name>/<start>`, as soon as its bytes have arrived.
//!   A put of a piece is atomic, so a piece exists only when all its bytes
//!   do.
//! - The last stretch of the file, between 1 and `piece` bytes and called
//!   the tail here, is never written as a piece. It goes straight into the
//!   finished object: one conditional put of `upload/<name>` whose body is
//!   the stored pieces read back in order, followed by the tail from the
//!   request. That put is the only write of the finished object, so R2
//!   computes its MD5 (bitwise validation reads it) and checks it against
//!   the client's `<md5_cksum>`. The pieces are deleted afterwards.
//! - The bytes held for an unfinished file are the grid pieces present
//!   without a gap from offset 0. Because the tail is never a piece, that
//!   number is always less than the file size, so `<get_file_size>` never
//!   tells a client its file is complete when it is not; a client that
//!   hears a size at least as large as its file counts the upload as done
//!   (`FILE_XFER_SET::poll` in `client/file_xfer.cpp`).
//!
//! A file no larger than one piece has no pieces: its upload is one head
//! and one put, as before resume existed.
//!
//! Offsets follow `copy_socket_to_file` in `sched/file_upload_handler.cpp`:
//! an offset beyond the bytes held is a transient error, after which the
//! client asks for the size again; an offset below them is accepted, and
//! the bytes the server already has are read and dropped (upstream writes
//! them over the same bytes). Unlike upstream, an offset at or past the end
//! of a file that is not stored is an error too, not a success: this
//! handler never answers success for a file it does not hold.
//!
//! Stale pieces (an upload that is never finished, or pieces written by a
//! request that lost a race) are removed by a bucket lifecycle rule on the
//! `partial/` prefix; see `docs/deploy.md`.

use async_trait::async_trait;
use cb_crypto::{verify_upload_certificate, PublicKey};
use cb_proto::upload::{is_valid_filename, UploadError, UploadHeader};

use crate::CoreResult;

/// Size of a stored piece. A cut-off upload loses at most this many bytes
/// beyond the last piece boundary it crossed.
pub const PIECE_BYTES: u64 = 8 * 1024 * 1024;

/// A name the bucket can store under `upload/` and `partial/`: valid as
/// upstream sees it, without directories, and at most 255 bytes.
pub fn is_flat_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && is_valid_filename(name) && !name.contains('/')
}

/// The name under the `partial/` prefix of the piece of `name` that starts
/// at byte `start`.
pub fn piece_name(name: &str, start: u64) -> String {
    format!("{name}/{start}")
}

/// Where the tail of a file of `nbytes` bytes starts: the last grid
/// boundary before its end. The tail is `nbytes - tail_start` bytes long,
/// between 1 and `piece` (0 for an empty file).
pub fn tail_start(nbytes: u64, piece: u64) -> u64 {
    if nbytes == 0 {
        0
    } else {
        (nbytes - 1) / piece * piece
    }
}

/// The bytes held: grid pieces of exactly `piece` bytes present without a
/// gap from 0, each ending at or before `limit`. `pieces` lists stored
/// pieces as (start, size).
pub fn held_bytes(pieces: &[(u64, u64)], piece: u64, limit: u64) -> u64 {
    let starts: std::collections::HashSet<u64> = pieces
        .iter()
        .filter(|(_, size)| *size == piece)
        .map(|(start, _)| *start)
        .collect();
    let mut held = 0u64;
    while let Some(end) = held.checked_add(piece) {
        if end > limit || !starts.contains(&held) {
            break;
        }
        held = end;
    }
    held
}

/// The 16 bytes of a hex MD5, or `None` if `text` is not one.
pub fn parse_md5(text: &str) -> Option<[u8; 16]> {
    let t = text.trim().as_bytes();
    if t.len() != 32 || !t.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, pair) in t.chunks(2).enumerate() {
        let s = std::str::from_utf8(pair).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

/// A byte count sent as a decimal number: finite, not negative, whole.
fn whole_bytes(x: f64) -> Option<u64> {
    (x.is_finite() && x >= 0.0 && x.fract() == 0.0 && x < 9.0e15).then_some(x as u64)
}

/// The checks `handle_file_upload` makes on the header before any data is
/// read: the name, the upload certificate against `key` unless
/// `ignore_upload_certificates`, the signed and the project size limits,
/// and a name the bucket can hold. They apply to every request, whatever
/// its offset.
pub fn check_header(
    header: &UploadHeader,
    ignore_upload_certificates: bool,
    key: Option<&PublicKey>,
    max_upload_nbytes: f64,
) -> Result<(), UploadError> {
    header.check_signature_fields(ignore_upload_certificates)?;
    if !ignore_upload_certificates {
        let Some(key) = key else {
            return Err(UploadError::transient("the project has no upload key"));
        };
        if !verify_upload_certificate(&header.name, header.max_nbytes, &header.xml_signature, key) {
            return Err(UploadHeader::invalid_signature());
        }
    }
    header.check_limits(ignore_upload_certificates, max_upload_nbytes)?;
    if !is_flat_name(&header.name) {
        return Err(UploadError::permanent("invalid filename"));
    }
    Ok(())
}

/// How the write of one piece ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceWrite {
    Stored,
    /// The request body ended or failed before the piece was complete.
    /// Nothing was stored.
    BodyEnded,
}

/// How the write of the finished file ended. Only `Stored` stores
/// anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompleteWrite {
    Stored,
    /// The file already exists; the conditional put did not replace it.
    Exists,
    /// The request body ended or failed before the tail was complete.
    BodyEnded,
    /// A piece could not be read back, or had the wrong size.
    PieceMissing,
    /// The store refused the object with the body and the pieces intact:
    /// the MD5 did not match the client's, or the put failed.
    Rejected,
}

/// The object store as the upload path needs it: `upload/<name>` for
/// finished files and `partial/<name>/<start>` for pieces. `Body` is the
/// unread rest of the request body.
#[async_trait(?Send)]
pub trait UploadStore {
    type Body;

    /// Size of the finished file, if it is stored.
    async fn complete_size(&self, name: &str) -> CoreResult<Option<u64>>;
    /// Whether the piece starting at `start` is stored.
    async fn has_piece(&self, name: &str, start: u64) -> CoreResult<bool>;
    /// Every stored piece of `name`, as (start, size), in any order.
    async fn list_pieces(&self, name: &str) -> CoreResult<Vec<(u64, u64)>>;
    /// Reads and drops `n` bytes of the body. False if it ended first.
    async fn skip(&self, body: &Self::Body, n: u64) -> CoreResult<bool>;
    /// Stores the next `len` bytes of the body as the piece at `start`,
    /// replacing a piece of the same name.
    async fn put_piece(
        &self,
        name: &str,
        start: u64,
        body: &Self::Body,
        len: u64,
    ) -> CoreResult<PieceWrite>;
    /// Stores the finished file, unless it exists: the pieces at `starts`,
    /// in that order, each `piece` bytes long, then the next `tail` bytes
    /// of the body. With `md5`, the store refuses contents with another
    /// MD5.
    async fn put_complete(
        &self,
        name: &str,
        starts: &[u64],
        piece: u64,
        body: &Self::Body,
        tail: u64,
        md5: Option<[u8; 16]>,
    ) -> CoreResult<CompleteWrite>;
    /// Deletes pieces; a missing piece is not an error.
    async fn delete_pieces(&self, name: &str, starts: &[u64]) -> CoreResult<()>;
}

/// The answer to `<get_file_size>` (`handle_get_file_size`): the size of the
/// finished file, or else the bytes held, 0 for a file never seen. The two
/// heads run together; a listing follows only when a first piece exists.
pub async fn file_size<S: UploadStore>(store: &S, name: &str, piece: u64) -> CoreResult<u64> {
    let (complete, first) =
        futures_util::future::join(store.complete_size(name), store.has_piece(name, 0)).await;
    if let Some(size) = complete? {
        return Ok(size);
    }
    if !first? {
        return Ok(0);
    }
    Ok(held_bytes(&store.list_pieces(name).await?, piece, u64::MAX))
}

fn exists_with_another_size(name: &str) -> UploadError {
    UploadError::permanent(format!("file {name} exists with another size"))
}

fn cut_off(name: &str, kept: u64) -> UploadError {
    UploadError::transient(format!(
        "the upload of {name} ended early; {kept} bytes are held"
    ))
}

/// Stores the data of an upload whose header passed [`check_header`]
/// (`handle_file_upload` and `copy_socket_to_file`). `body` holds the file
/// bytes from `header.offset` on.
///
/// `Ok(Ok(()))` is a success reply, `Ok(Err(e))` the error reply `e`, and
/// `Err` a store failure, which the caller answers with a transient error.
pub async fn receive<S: UploadStore>(
    store: &S,
    header: &UploadHeader,
    body: &S::Body,
    piece: u64,
) -> CoreResult<Result<(), UploadError>> {
    let name = header.name.as_str();
    let nbytes = header.nbytes as u64;
    let Some(offset) = whole_bytes(header.offset) else {
        return Ok(Err(UploadError::transient(format!(
            "can't resume partial file {name}: bad offset"
        ))));
    };
    // A stored file is never replaced or appended to: the upload was
    // complete, and the result may have been validated against it since.
    if let Some(size) = store.complete_size(name).await? {
        return Ok(if size == nbytes {
            Ok(())
        } else {
            Err(exists_with_another_size(name))
        });
    }
    let last = tail_start(nbytes, piece);
    // An upload from the start writes every piece, so it needs no listing.
    let start = if offset == 0 {
        0
    } else {
        let held = held_bytes(&store.list_pieces(name).await?, piece, last);
        if offset > held {
            return Ok(Err(UploadError::transient(format!(
                "length of file {name} {held} bytes < offset {offset} bytes"
            ))));
        }
        held
    };
    if start > offset && !store.skip(body, start - offset).await? {
        return Ok(Err(cut_off(name, start)));
    }
    let mut at = start;
    while at < last {
        match store.put_piece(name, at, body, piece).await? {
            PieceWrite::Stored => at += piece,
            PieceWrite::BodyEnded => return Ok(Err(cut_off(name, at))),
        }
    }
    let starts: Vec<u64> = (0..last / piece).map(|k| k * piece).collect();
    let md5 = parse_md5(&header.md5_cksum);
    match store
        .put_complete(name, &starts, piece, body, nbytes - last, md5)
        .await?
    {
        CompleteWrite::Stored => {
            if !starts.is_empty() {
                // Pieces left by a failed delete expire under the lifecycle
                // rule; the upload itself succeeded.
                let _ = store.delete_pieces(name, &starts).await;
            }
            Ok(Ok(()))
        }
        // Another request stored the file first. A retry of the same
        // upload is answered as the check above answers it.
        CompleteWrite::Exists => Ok(match store.complete_size(name).await? {
            Some(size) if size == nbytes => Ok(()),
            _ => Err(exists_with_another_size(name)),
        }),
        CompleteWrite::BodyEnded => Ok(Err(cut_off(name, last))),
        // Expired or deleted by a request that finished the file. The next
        // size query sees what is left.
        CompleteWrite::PieceMissing => Ok(Err(UploadError::transient(format!(
            "part of file {name} is no longer held; ask for its size again"
        )))),
        CompleteWrite::Rejected => {
            if store.complete_size(name).await? == Some(nbytes) {
                return Ok(Ok(()));
            }
            if starts.is_empty() {
                return Ok(Err(UploadError::transient("the file could not be stored")));
            }
            // The pieces do not make the file the client describes (most
            // likely its MD5 differs). Keeping them would fail the same way
            // on every retry, so the next attempt starts from 0.
            store.delete_pieces(name, &starts).await?;
            Ok(Err(UploadError::transient(format!(
                "file {name} could not be stored; send it again from the start"
            ))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tails() {
        assert_eq!(tail_start(0, 16), 0);
        assert_eq!(tail_start(1, 16), 0);
        assert_eq!(tail_start(16, 16), 0);
        assert_eq!(tail_start(17, 16), 16);
        assert_eq!(tail_start(32, 16), 16);
        assert_eq!(tail_start(33, 16), 32);
        assert_eq!(tail_start(PIECE_BYTES, PIECE_BYTES), 0);
        assert_eq!(tail_start(PIECE_BYTES + 1, PIECE_BYTES), PIECE_BYTES);
    }

    #[test]
    fn held() {
        assert_eq!(held_bytes(&[], 16, u64::MAX), 0);
        assert_eq!(held_bytes(&[(16, 16)], 16, u64::MAX), 0);
        assert_eq!(held_bytes(&[(32, 16), (0, 16), (16, 16)], 16, u64::MAX), 48);
        // A gap ends what is held.
        assert_eq!(held_bytes(&[(0, 16), (32, 16)], 16, u64::MAX), 16);
        // A piece of another size (another piece size, or a stray object)
        // does not count.
        assert_eq!(held_bytes(&[(0, 16), (16, 15)], 16, u64::MAX), 16);
        // Pieces past the tail of this file do not count.
        assert_eq!(held_bytes(&[(0, 16), (16, 16), (32, 16)], 16, 32), 32);
        assert_eq!(held_bytes(&[(0, 16)], 16, 15), 0);
    }

    #[test]
    fn md5s() {
        let m = parse_md5("d41d8cd98f00b204e9800998ecf8427e").unwrap();
        assert_eq!(m[0], 0xd4);
        assert_eq!(m[15], 0x7e);
        assert_eq!(parse_md5(" D41D8CD98F00B204E9800998ECF8427E\n"), Some(m));
        assert_eq!(parse_md5(""), None);
        assert_eq!(parse_md5("d41d8cd98f00b204e9800998ecf8427"), None);
        assert_eq!(parse_md5("g41d8cd98f00b204e9800998ecf8427e"), None);
        assert_eq!(parse_md5("+41d8cd98f00b204e9800998ecf8427e"), None);
    }

    #[test]
    fn offsets() {
        assert_eq!(whole_bytes(0.0), Some(0));
        assert_eq!(whole_bytes(8388608.0), Some(8388608));
        assert_eq!(whole_bytes(-1.0), None);
        assert_eq!(whole_bytes(1.5), None);
        assert_eq!(whole_bytes(f64::NAN), None);
        assert_eq!(whole_bytes(f64::INFINITY), None);
    }

    #[test]
    fn names() {
        assert!(is_flat_name("wu_1_0_r77_0"));
        assert!(!is_flat_name(""));
        assert!(!is_flat_name("a/b"));
        assert!(!is_flat_name("../a"));
        assert!(!is_flat_name(&"x".repeat(256)));
        assert_eq!(piece_name("f", 8388608), "f/8388608");
    }
}
