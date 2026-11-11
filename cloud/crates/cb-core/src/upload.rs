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
//!   object, `partial/<name>/<md5>/<start>`, as soon as its bytes have
//!   arrived. A put of a piece is atomic, so a piece exists only when all
//!   its bytes do. `<md5>` is the client's whole-file `<md5_cksum>`: pieces
//!   of two files that share a name (a task recomputed after it was lost
//!   can write another file under the name its first attempt used) never
//!   share a key, and a request reuses only the pieces under its own MD5.
//!   A request whose `<md5_cksum>` is missing or not an MD5 writes no
//!   pieces: its file goes into one put, and no earlier piece is reused.
//! - The last stretch of the file, between 1 and `piece` bytes and called
//!   the tail here, is never written as a piece. It goes straight into the
//!   finished object: one conditional put of `upload/<name>` whose body is
//!   the stored pieces read back in order, followed by the tail from the
//!   request. That put is the only write of the finished object, so R2
//!   computes its MD5 (bitwise validation reads it) and checks it against
//!   the client's `<md5_cksum>`; pieces are only ever composed with that
//!   check on. The pieces are deleted afterwards.
//! - The bytes held for an unfinished file are the grid pieces present
//!   without a gap from offset 0. Because the tail is never a piece, that
//!   number is always less than the size of the file that wrote the pieces.
//!   A `<get_file_size>` request carries only the name, so it cannot be
//!   checked against the file the client is about to send: a client that
//!   hears a size at least as large as its file counts the upload as done
//!   (`FILE_XFER_SET::poll` in `client/file_xfer.cpp`), and a name whose
//!   pieces came from a longer file of an earlier attempt can make that
//!   happen for a shorter recomputed file. The stock handler has the same
//!   property with the bytes on disk. Nothing is then stored under the
//!   finished name, so the result fails validation and is retried; no
//!   wrong file is ever served as the output. A request that does arrive
//!   removes the pieces of any other file under its name.
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

/// The 32 lowercase hex digits of an MD5, as used in piece names.
pub fn md5_to_hex(md5: &[u8; 16]) -> String {
    md5.iter().map(|b| format!("{b:02x}")).collect()
}

/// The name under the `partial/` prefix of the piece of `name`, for the file
/// with this MD5, that starts at byte `start`.
pub fn piece_name(name: &str, md5: &[u8; 16], start: u64) -> String {
    format!("{name}/{}/{start}", md5_to_hex(md5))
}

/// Splits what follows `<name>/` in a piece's name into its MD5 and start;
/// `None` for a name that is not a piece's.
pub fn parse_piece_name(rest: &str) -> Option<([u8; 16], u64)> {
    let (md5, start) = rest.split_once('/')?;
    if md5.len() != 32 || !md5.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    if start.is_empty() || !start.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((parse_md5(md5)?, start.parse().ok()?))
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

/// A finished file as the store holds it: its size and, when the store
/// has it, its MD5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredFile {
    pub size: u64,
    pub md5: Option<[u8; 16]>,
}

impl StoredFile {
    /// Whether this is the file a request describes: its size, and its MD5
    /// when both sides have one. A retry of the same upload matches; a
    /// request for another file under the name does not, so it is never
    /// answered with success for data the store does not hold.
    pub fn is(&self, nbytes: u64, md5: Option<[u8; 16]>) -> bool {
        self.size == nbytes && !matches!((self.md5, md5), (Some(a), Some(b)) if a != b)
    }
}

/// A stored piece: the file's MD5 as its client sent it, where the piece
/// starts, and its size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPiece {
    pub md5: [u8; 16],
    pub start: u64,
    pub size: u64,
}

/// The object store as the upload path needs it: `upload/<name>` for
/// finished files and `partial/<name>/<md5>/<start>` for pieces. `Body` is
/// the unread rest of the request body.
#[async_trait(?Send)]
pub trait UploadStore {
    type Body;

    /// The finished file, if it is stored.
    async fn complete(&self, name: &str) -> CoreResult<Option<StoredFile>>;
    /// Every stored piece of `name`, whatever file wrote it, in any order.
    async fn list_pieces(&self, name: &str) -> CoreResult<Vec<StoredPiece>>;
    /// Reads and drops `n` bytes of the body. False if it ended first.
    async fn skip(&self, body: &Self::Body, n: u64) -> CoreResult<bool>;
    /// Stores the next `len` bytes of the body as the piece at `start` of
    /// the file with this `md5`, replacing a piece of the same name.
    async fn put_piece(
        &self,
        name: &str,
        md5: &[u8; 16],
        start: u64,
        body: &Self::Body,
        len: u64,
    ) -> CoreResult<PieceWrite>;
    /// Stores the finished file, unless it exists: the pieces at `starts`
    /// (of the file with this `md5`, which is then present), in that order,
    /// each `piece` bytes long, then the next `tail` bytes of the body.
    /// With `md5`, the store refuses contents with another MD5.
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
    async fn delete_pieces(&self, name: &str, pieces: &[StoredPiece]) -> CoreResult<()>;
}

/// The answer to `<get_file_size>` (`handle_get_file_size`): the size of the
/// finished file, or else the bytes held, 0 for a file never seen. The head
/// and the listing run together.
///
/// The request names the file and nothing else, so the answer cannot depend
/// on the file the client will send. When pieces of more than one file are
/// present, which one the client has is unknown and the answer is 0.
pub async fn file_size<S: UploadStore>(store: &S, name: &str, piece: u64) -> CoreResult<u64> {
    let (complete, listed) =
        futures_util::future::join(store.complete(name), store.list_pieces(name)).await;
    if let Some(file) = complete? {
        return Ok(file.size);
    }
    let listed = listed?;
    let Some(first) = listed.first() else {
        return Ok(0);
    };
    if listed.iter().any(|p| p.md5 != first.md5) {
        return Ok(0);
    }
    Ok(held_of(&listed, piece, u64::MAX))
}

fn held_of(pieces: &[StoredPiece], piece: u64, limit: u64) -> u64 {
    let v: Vec<(u64, u64)> = pieces.iter().map(|p| (p.start, p.size)).collect();
    held_bytes(&v, piece, limit)
}

fn exists_with_other_contents(name: &str) -> UploadError {
    UploadError::permanent(format!("file {name} exists with other contents"))
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
    let md5 = parse_md5(&header.md5_cksum);
    // A stored file is never replaced or appended to: the upload was
    // complete, and the result may have been validated against it since.
    // Success only for the same file (size and MD5), as a retry sends it.
    if let Some(file) = store.complete(name).await? {
        return Ok(if file.is(nbytes, md5) {
            Ok(())
        } else {
            Err(exists_with_other_contents(name))
        });
    }
    let last = tail_start(nbytes, piece);
    // Pieces are kept, and reused, only under a whole-file MD5 that the
    // finished object can be checked against.
    let mut held = 0;
    let mut dropped_others = false;
    if last > 0 || offset > 0 {
        let (mine, others): (Vec<StoredPiece>, Vec<StoredPiece>) = store
            .list_pieces(name)
            .await?
            .into_iter()
            .partition(|p| Some(p.md5) == md5);
        // Pieces of another file under this name are never composed into
        // this one. Dropping them also keeps a later size query from
        // reporting bytes this client does not have.
        if !others.is_empty() {
            store.delete_pieces(name, &others).await?;
            dropped_others = true;
        }
        held = held_of(&mine, piece, last);
    }
    // An upload from the start writes every piece.
    let start = if offset == 0 {
        0
    } else {
        if offset > held {
            return Ok(Err(if dropped_others {
                UploadError::transient(format!(
                    "pieces of file {name} came from other contents and were dropped; \
                     send it again from the start"
                ))
            } else {
                UploadError::transient(format!(
                    "length of file {name} {held} bytes < offset {offset} bytes"
                ))
            }));
        }
        held
    };
    if start > offset && !store.skip(body, start - offset).await? {
        return Ok(Err(cut_off(name, start)));
    }
    // With no MD5 the whole file is the tail: one put, no pieces.
    let (last, tail) = match md5 {
        Some(_) => (last, nbytes - last),
        None => (0, nbytes),
    };
    let mut at = start;
    if let Some(m) = &md5 {
        while at < last {
            match store.put_piece(name, m, at, body, piece).await? {
                PieceWrite::Stored => at += piece,
                PieceWrite::BodyEnded => return Ok(Err(cut_off(name, at))),
            }
        }
    }
    let starts: Vec<u64> = (0..last / piece).map(|k| k * piece).collect();
    let mine = |starts: &[u64]| -> Vec<StoredPiece> {
        starts
            .iter()
            .filter_map(|&start| {
                md5.map(|md5| StoredPiece {
                    md5,
                    start,
                    size: piece,
                })
            })
            .collect()
    };
    match store
        .put_complete(name, &starts, piece, body, tail, md5)
        .await?
    {
        CompleteWrite::Stored => {
            if !starts.is_empty() {
                // Pieces left by a failed delete expire under the lifecycle
                // rule; the upload itself succeeded.
                let _ = store.delete_pieces(name, &mine(&starts)).await;
            }
            Ok(Ok(()))
        }
        // Another request stored the file first. A retry of the same
        // upload is answered as the check above answers it.
        CompleteWrite::Exists => Ok(match store.complete(name).await? {
            Some(file) if file.is(nbytes, md5) => Ok(()),
            _ => Err(exists_with_other_contents(name)),
        }),
        CompleteWrite::BodyEnded => Ok(Err(cut_off(name, last))),
        // Expired or deleted by a request that finished the file. The next
        // size query sees what is left.
        CompleteWrite::PieceMissing => Ok(Err(UploadError::transient(format!(
            "part of file {name} is no longer held; ask for its size again"
        )))),
        CompleteWrite::Rejected => {
            if store
                .complete(name)
                .await?
                .is_some_and(|f| f.is(nbytes, md5))
            {
                return Ok(Ok(()));
            }
            if starts.is_empty() {
                return Ok(Err(UploadError::transient("the file could not be stored")));
            }
            // The pieces do not make the file the client describes (most
            // likely its MD5 differs). Keeping them would fail the same way
            // on every retry, so the next attempt starts from 0.
            store.delete_pieces(name, &mine(&starts)).await?;
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
        let m = parse_md5("d41d8cd98f00b204e9800998ecf8427e").unwrap();
        assert_eq!(
            piece_name("f", &m, 8388608),
            "f/d41d8cd98f00b204e9800998ecf8427e/8388608"
        );
        assert_eq!(
            parse_piece_name("d41d8cd98f00b204e9800998ecf8427e/8388608"),
            Some((m, 8388608))
        );
        // Not pieces: the old layout, a stray object, upper case, a bad
        // start.
        assert_eq!(parse_piece_name("8388608"), None);
        assert_eq!(parse_piece_name("d41d8cd98f00b204e9800998ecf8427e/"), None);
        assert_eq!(parse_piece_name("D41D8CD98F00B204E9800998ECF8427E/1"), None);
        assert_eq!(
            parse_piece_name("d41d8cd98f00b204e9800998ecf8427e/1/2"),
            None
        );
        assert_eq!(
            parse_piece_name("d41d8cd98f00b204e9800998ecf8427e/-1"),
            None
        );
    }
}
