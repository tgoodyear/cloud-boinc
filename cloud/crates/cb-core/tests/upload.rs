//! Uploads with resume (`cb_core::upload`) against an in-memory bucket.
//!
//! The pieces here are 16 bytes and most files 100, so a file has six
//! stored pieces (0 to 95) and a 4-byte tail. A cut request body stands for
//! both ways an upload is interrupted: the body ends early, or the client
//! disconnects and the Worker stops; either way only the pieces stored
//! before the cut remain.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use async_trait::async_trait;
use cb_core::upload::{
    check_header, file_size, md5_to_hex, parse_piece_name, receive, CompleteWrite, PieceRun,
    PieceWrite, StoredFile, StoredPiece, UploadStore, PIECE_BYTES,
};
use cb_core::{CoreResult, FileKind};
use cb_crypto::md5_hex;
use cb_db::{stmt, Db};
use cb_proto::upload::{UploadError, UploadHeader};
use cb_sqlite::SqliteDb;
use futures_lite::future::block_on;

const P: u64 = 16;
const NAME: &str = "wu_1_0_r9_0";

/// The request body after the header: file bytes from the request's
/// offset, ending at `end` when the transfer is cut.
struct MemBody {
    data: Vec<u8>,
    pos: Cell<usize>,
    end: usize,
}

impl MemBody {
    fn read(&self, n: u64) -> Option<Vec<u8>> {
        let pos = self.pos.get();
        let want = pos + n as usize;
        if want > self.end {
            self.pos.set(self.end);
            return None;
        }
        self.pos.set(want);
        Some(self.data[pos..want].to_vec())
    }
}

type Hook = Box<dyn FnOnce(&MemStore)>;
type PieceHook = Box<dyn FnMut(&MemStore, usize)>;

struct MemStore {
    objects: RefCell<BTreeMap<String, Vec<u8>>>,
    ops: RefCell<Vec<&'static str>>,
    /// Runs when the next finished-file write starts.
    before_complete: RefCell<Option<Hook>>,
    /// Runs when each piece write starts, with the number of piece writes
    /// so far.
    before_piece: RefCell<Option<PieceHook>>,
    pieces_put: Cell<usize>,
    /// The project database, with no result in it until a test adds one.
    db: SqliteDb,
}

impl Default for MemStore {
    fn default() -> Self {
        MemStore {
            objects: Default::default(),
            ops: Default::default(),
            before_complete: Default::default(),
            before_piece: Default::default(),
            pieces_put: Cell::new(0),
            db: SqliteDb::with_schema().unwrap(),
        }
    }
}

fn upload_key(name: &str) -> String {
    format!("{}{name}", FileKind::Upload.prefix())
}

/// The key of the piece at `start` of the file `data`, as its client
/// describes it with `md5_cksum`, in the first attempt.
fn piece_key(name: &str, data: &[u8], start: u64) -> String {
    format!(
        "{}{name}/0/{}/{start}",
        FileKind::Partial.prefix(),
        md5_hex(data)
    )
}

fn piece_key_md5(name: &str, epoch: u64, md5: &[u8; 16], start: u64) -> String {
    format!(
        "{}{name}/{epoch}/{}/{start}",
        FileKind::Partial.prefix(),
        md5_to_hex(md5)
    )
}

impl MemStore {
    fn op(&self, op: &'static str) {
        self.ops.borrow_mut().push(op);
    }

    fn take_ops(&self) -> Vec<&'static str> {
        std::mem::take(&mut *self.ops.borrow_mut())
    }

    fn count(ops: &[&str], op: &str) -> usize {
        ops.iter().filter(|o| **o == op).count()
    }

    fn complete(&self, name: &str) -> Option<Vec<u8>> {
        self.objects.borrow().get(&upload_key(name)).cloned()
    }

    fn pieces(&self) -> Vec<String> {
        let prefix = FileKind::Partial.prefix();
        self.objects
            .borrow()
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect()
    }

    /// Bytes from 0 that stored pieces of the file `data` really hold.
    fn stored_from_zero(&self, name: &str, data: &[u8]) -> u64 {
        let mut at = 0;
        while let Some(p) = self.objects.borrow().get(&piece_key(name, data, at)) {
            at += p.len() as u64;
        }
        at
    }

    fn remove(&self, key: &str) {
        self.objects.borrow_mut().remove(key);
    }

    /// The scheduler sent the result of the file `name` again, as its
    /// resend records it for each upload name of the result.
    fn resend(&self, name: &str) {
        block_on(self.db.run(stmt!(
            "INSERT INTO upload_epoch (name, result_id, epoch) VALUES (?, 1, 1) \
             ON CONFLICT (name) DO UPDATE SET epoch = upload_epoch.epoch + 1",
            name
        )))
        .unwrap();
    }

    /// Database statements run so far (the lookup of where the attempt starts).
    fn db_statements(&self) -> u64 {
        self.db.statements()
    }
}

#[async_trait(?Send)]
impl UploadStore for MemStore {
    type Body = MemBody;

    async fn complete(&self, name: &str) -> CoreResult<Option<StoredFile>> {
        self.op("head");
        Ok(MemStore::complete(self, name).map(|d| StoredFile {
            size: d.len() as u64,
            md5: cb_core::upload::parse_md5(&md5_hex(&d)),
        }))
    }

    async fn list_pieces(&self, name: &str) -> CoreResult<Vec<StoredPiece>> {
        self.op("list");
        let prefix = format!("{}{name}/", FileKind::Partial.prefix());
        Ok(self
            .objects
            .borrow()
            .iter()
            .filter_map(|(k, v)| {
                let (epoch, md5, start) = parse_piece_name(k.strip_prefix(&prefix)?)?;
                Some(StoredPiece {
                    epoch,
                    md5,
                    start,
                    size: v.len() as u64,
                })
            })
            .collect())
    }

    async fn skip(&self, body: &MemBody, n: u64) -> CoreResult<bool> {
        Ok(body.read(n).is_some())
    }

    async fn put_piece(
        &self,
        name: &str,
        epoch: u64,
        md5: &[u8; 16],
        start: u64,
        body: &MemBody,
        len: u64,
    ) -> CoreResult<PieceWrite> {
        self.op("put");
        let n = self.pieces_put.get();
        self.pieces_put.set(n + 1);
        let hook = self.before_piece.borrow_mut().take();
        if let Some(mut hook) = hook {
            hook(self, n);
            *self.before_piece.borrow_mut() = Some(hook);
        }
        let Some(bytes) = body.read(len) else {
            return Ok(PieceWrite::BodyEnded);
        };
        let key = piece_key_md5(name, epoch, md5, start);
        self.objects.borrow_mut().insert(key, bytes);
        Ok(PieceWrite::Stored)
    }

    async fn put_complete(
        &self,
        run: &PieceRun<'_>,
        body: &MemBody,
        tail: u64,
        md5: Option<[u8; 16]>,
    ) -> CoreResult<CompleteWrite> {
        let PieceRun {
            name,
            epoch,
            starts,
            piece,
        } = *run;
        self.op("put");
        if let Some(hook) = self.before_complete.borrow_mut().take() {
            hook(self);
        }
        if self.complete(name).is_some() {
            return Ok(CompleteWrite::Exists);
        }
        let mut data = Vec::new();
        for s in starts {
            self.op("get");
            let key = piece_key_md5(
                name,
                epoch,
                &md5.expect("pieces are composed with an MD5"),
                *s,
            );
            match self.objects.borrow().get(&key) {
                Some(p) if p.len() as u64 == piece => data.extend_from_slice(p),
                _ => return Ok(CompleteWrite::PieceMissing),
            }
        }
        let Some(t) = body.read(tail) else {
            return Ok(CompleteWrite::BodyEnded);
        };
        data.extend_from_slice(&t);
        if let Some(m) = md5 {
            let hex: String = m.iter().map(|b| format!("{b:02x}")).collect();
            if hex != md5_hex(&data) {
                return Ok(CompleteWrite::Rejected);
            }
        }
        self.objects.borrow_mut().insert(upload_key(name), data);
        Ok(CompleteWrite::Stored)
    }

    async fn delete_pieces(&self, name: &str, pieces: &[StoredPiece]) -> CoreResult<()> {
        self.op("delete");
        for p in pieces {
            self.remove(&piece_key_md5(name, p.epoch, &p.md5, p.start));
        }
        Ok(())
    }
}

fn file(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u32 * 31 + seed as u32 * 7 + 3) as u8)
        .collect()
}

fn header(name: &str, data: &[u8], offset: f64) -> UploadHeader {
    UploadHeader {
        client_version: Default::default(),
        name: name.to_string(),
        xml_signature: String::new(),
        max_nbytes: 1e6,
        nbytes: data.len() as f64,
        offset,
        md5_cksum: md5_hex(data),
        data_offset: 0,
    }
}

/// One upload request with the file data from `offset`, cut at the
/// absolute byte `cut` if given, as a client sends it.
fn send_with(
    store: &MemStore,
    h: &UploadHeader,
    data: &[u8],
    cut: Option<usize>,
    piece: u64,
) -> Result<(), UploadError> {
    let from = (h.offset.max(0.0) as usize).min(data.len());
    let body = MemBody {
        data: data[from..].to_vec(),
        pos: Cell::new(0),
        end: cut.map_or(data.len(), |c| c.max(from)) - from,
    };
    block_on(receive(store, &store.db, h, &body, piece)).unwrap()
}

fn send(store: &MemStore, data: &[u8], offset: u64, cut: Option<usize>) -> Result<(), UploadError> {
    send_with(store, &header(NAME, data, offset as f64), data, cut, P)
}

fn size(store: &MemStore) -> u64 {
    block_on(file_size(store, &store.db, NAME, P)).unwrap()
}

#[test]
fn one_shot_upload_costs_a_head_and_a_put() {
    // At or below one piece: no piece, no listing, no read back.
    for len in [0usize, 1, 15, 16] {
        let store = MemStore::default();
        let data = file(len, 1);
        assert_eq!(send(&store, &data, 0, None), Ok(()));
        assert_eq!(store.take_ops(), ["head", "put"], "{len} bytes");
        assert_eq!(store.complete(NAME), Some(data));
        assert!(store.pieces().is_empty());
    }
    // The real piece size, with a typical output.
    let store = MemStore::default();
    let data = file(200_000, 2);
    let h = header(NAME, &data, 0.0);
    assert_eq!(send_with(&store, &h, &data, None, PIECE_BYTES), Ok(()));
    assert_eq!(store.take_ops(), ["head", "put"]);
    assert_eq!(store.complete(NAME), Some(data));

    // A size query for a file never seen: a head and a listing.
    let store = MemStore::default();
    assert_eq!(size(&store), 0);
    assert_eq!(store.take_ops(), ["head", "list"]);
}

#[test]
fn large_upload_writes_pieces_then_one_object() {
    let store = MemStore::default();
    let data = file(100, 3);
    assert_eq!(send(&store, &data, 0, None), Ok(()));
    let ops = store.take_ops();
    // Head, a listing (for pieces of other files), six pieces, the finished
    // file (reading the six back), one delete of the pieces.
    assert_eq!(MemStore::count(&ops, "head"), 1);
    assert_eq!(MemStore::count(&ops, "put"), 7);
    assert_eq!(MemStore::count(&ops, "get"), 6);
    assert_eq!(MemStore::count(&ops, "delete"), 1);
    assert_eq!(MemStore::count(&ops, "list"), 1);
    assert_eq!(store.complete(NAME), Some(data));
    assert!(store.pieces().is_empty());
    assert_eq!(size(&store), 100);
}

#[test]
fn resume_after_a_cut_gives_identical_bytes() {
    let data = file(100, 4);
    for cut in [0usize, 1, 15, 16, 17, 31, 32, 47, 48, 63, 95, 96, 97, 99] {
        let store = MemStore::default();
        let e = send(&store, &data, 0, Some(cut)).unwrap_err();
        assert!(e.transient, "cut at {cut}: {e}");
        assert!(store.complete(NAME).is_none());
        // Whole pieces before the cut are held; the tail never is.
        let held = size(&store);
        assert_eq!(held, (cut as u64 / P * P).min(96), "cut at {cut}");
        assert!(held <= store.stored_from_zero(NAME, &data));
        assert!(held < data.len() as u64);

        store.take_ops();
        assert_eq!(send(&store, &data, held, None), Ok(()), "cut at {cut}");
        let ops = store.take_ops();
        if held > 0 {
            // A resumed request lists the pieces once.
            assert_eq!(MemStore::count(&ops, "list"), 1);
            assert_eq!(MemStore::count(&ops, "head"), 1);
        }
        assert_eq!(store.complete(NAME), Some(data.clone()), "cut at {cut}");
        assert!(store.pieces().is_empty());
        assert_eq!(size(&store), 100);
    }
}

#[test]
fn several_cuts_in_a_row() {
    let store = MemStore::default();
    let data = file(100, 5);
    let mut offset = 0;
    for cut in [20usize, 50, 90, 98] {
        assert!(send(&store, &data, offset, Some(cut)).is_err());
        let held = size(&store);
        assert!(held >= offset && held <= cut as u64);
        offset = held;
    }
    assert_eq!(offset, 96);
    assert_eq!(send(&store, &data, offset, None), Ok(()));
    assert_eq!(store.complete(NAME), Some(data));
}

#[test]
fn offset_below_what_is_held_is_accepted() {
    // Upstream seeks back and writes the same bytes again; here the bytes
    // already held are read and dropped.
    let data = file(100, 6);
    for offset in [0u64, 1, 20, 32, 63] {
        let store = MemStore::default();
        assert!(send(&store, &data, 0, Some(70)).is_err());
        assert_eq!(size(&store), 64);
        assert_eq!(send(&store, &data, offset, None), Ok(()), "offset {offset}");
        assert_eq!(store.complete(NAME), Some(data.clone()));
        assert!(store.pieces().is_empty());
    }
}

#[test]
fn offset_beyond_what_is_held_is_a_transient_error() {
    let data = file(100, 7);
    let store = MemStore::default();
    assert!(send(&store, &data, 0, Some(40)).is_err());
    assert_eq!(size(&store), 32);
    let before = store.objects.borrow().clone();
    // As upstream: the client asks for the size again after this.
    let e = send(&store, &data, 48, None).unwrap_err();
    assert_eq!(
        e,
        UploadError::transient(format!("length of file {NAME} 32 bytes < offset 48 bytes"))
    );
    // Upstream answers success to an offset at or past the end; this
    // handler does not, since it does not hold the file.
    for offset in [100u64, 200] {
        let h = header(NAME, &data, offset as f64);
        let body = MemBody {
            data: Vec::new(),
            pos: Cell::new(0),
            end: 0,
        };
        let e = block_on(receive(&store, &store.db, &h, &body, P))
            .unwrap()
            .unwrap_err();
        assert!(e.transient, "{e}");
    }
    assert_eq!(*store.objects.borrow(), before);
    assert_eq!(size(&store), 32);
}

#[test]
fn bad_offsets_are_refused() {
    let data = file(100, 8);
    for offset in [-1.0, 1.5, f64::NAN, f64::INFINITY] {
        let store = MemStore::default();
        let h = header(NAME, &data, offset);
        let e = send_with(&store, &h, &data, None, P).unwrap_err();
        assert!(e.transient);
        assert!(e.message.contains("bad offset"), "{e}");
        assert!(store.objects.borrow().is_empty());
    }
}

#[test]
fn a_finished_file_is_never_appended_to() {
    let data = file(100, 9);
    let store = MemStore::default();
    assert_eq!(send(&store, &data, 0, None), Ok(()));
    store.take_ops();

    // A retry of the same file, from any offset: success, one head, no
    // byte read or written.
    for offset in [0u64, 48, 96] {
        assert_eq!(send(&store, &data, offset, None), Ok(()));
        assert_eq!(store.take_ops(), ["head"]);
    }
    // Other contents of the same size do not replace it, and are not told
    // they were stored: the file the client has is not the one held.
    let other = file(100, 10);
    assert_eq!(
        send(&store, &other, 0, None),
        Err(UploadError::permanent(format!(
            "file {NAME} exists with other contents"
        )))
    );
    assert_eq!(store.complete(NAME), Some(data.clone()));
    // A request that gives no MD5 is judged by size alone.
    let mut h = header(NAME, &other, 0.0);
    h.md5_cksum = String::new();
    assert_eq!(send_with(&store, &h, &other, None, P), Ok(()));
    assert_eq!(store.complete(NAME), Some(data.clone()));

    // A longer file under the same name, resumed past the old end.
    let longer = file(150, 9);
    let e = send(&store, &longer, 100, None).unwrap_err();
    assert_eq!(
        e,
        UploadError::permanent(format!("file {NAME} exists with other contents"))
    );
    assert_eq!(store.complete(NAME), Some(data));
    assert!(store.pieces().is_empty());
}

#[test]
fn size_limits_hold_for_resumed_requests() {
    let data = file(100, 11);
    // The project limit, without certificates.
    let mut h = header(NAME, &data, 64.0);
    h.nbytes = 120.0;
    let e = check_header(&h, true, None, 100.0).unwrap_err();
    assert!(!e.transient);
    assert!(e.message.contains("project limit"), "{e}");
    h.nbytes = 100.0;
    assert_eq!(check_header(&h, true, None, 100.0), Ok(()));

    // The signed limit binds every request, whatever its offset.
    let (private, public) = cb_crypto::generate_key_pair().unwrap();
    let mut h = header(NAME, &data, 64.0);
    h.max_nbytes = 100.0;
    h.xml_signature = cb_crypto::sign_upload_certificate(NAME, 100.0, &private).unwrap();
    assert_eq!(check_header(&h, false, Some(&public), 1e9), Ok(()));
    h.nbytes = 101.0;
    let e = check_header(&h, false, Some(&public), 1e9).unwrap_err();
    assert!(e.message.contains("exceeds limit"), "{e}");
    h.nbytes = 100.0;
    h.max_nbytes = 1000.0;
    assert_eq!(
        check_header(&h, false, Some(&public), 1e9),
        Err(UploadHeader::invalid_signature())
    );
    h.max_nbytes = 100.0;
    assert_eq!(
        check_header(&h, false, None, 1e9),
        Err(UploadError::transient("the project has no upload key"))
    );
    let mut bad = h.clone();
    bad.name = "a/b".into();
    bad.xml_signature = cb_crypto::sign_upload_certificate("a/b", 100.0, &private).unwrap();
    assert_eq!(
        check_header(&bad, false, Some(&public), 1e9),
        Err(UploadError::permanent("invalid filename"))
    );

    // The finished file is exactly as long as the request says, whatever
    // pieces an earlier, longer attempt left: they are not reused, so
    // the request that has the whole short file sends it from the start.
    let store = MemStore::default();
    assert!(send(&store, &data, 0, Some(99)).is_err());
    assert_eq!(size(&store), 96);
    let short = data[..40].to_vec();
    assert!(send(&store, &short, 32, None).is_err());
    assert_eq!(send(&store, &short, 0, None), Ok(()));
    assert_eq!(store.complete(NAME), Some(short));
}

#[test]
fn contents_that_do_not_match_start_over() {
    let a = file(100, 12);
    let b = file(100, 13);
    let store = MemStore::default();
    assert!(send(&store, &a, 0, Some(50)).is_err());
    assert_eq!(size(&store), 48);
    // Another file under the same name, resumed at the offset the size
    // query gave: the pieces are not b's, so they go and the client starts
    // again from 0.
    let e = send(&store, &b, 48, None).unwrap_err();
    assert!(e.transient);
    assert!(e.message.contains("from the start"), "{e}");
    assert!(store.complete(NAME).is_none());
    assert!(store.pieces().is_empty());
    assert_eq!(size(&store), 0);
    assert_eq!(send(&store, &b, 0, None), Ok(()));
    assert_eq!(store.complete(NAME), Some(b));
}

#[test]
fn stale_pieces() {
    let data = file(100, 14);
    // The lifecycle rule removes the oldest piece first.
    let store = MemStore::default();
    assert!(send(&store, &data, 0, Some(70)).is_err());
    store.remove(&piece_key(NAME, &data, 0));
    assert_eq!(size(&store), 0);
    assert!(send(&store, &data, 64, None).unwrap_err().transient);
    assert_eq!(send(&store, &data, 0, None), Ok(()));
    assert_eq!(store.complete(NAME), Some(data.clone()));
    assert!(store.pieces().is_empty());

    // A gap ends what is held.
    let store = MemStore::default();
    assert!(send(&store, &data, 0, Some(70)).is_err());
    store.remove(&piece_key(NAME, &data, 32));
    assert_eq!(size(&store), 32);
    assert_eq!(send(&store, &data, 32, None), Ok(()));
    assert_eq!(store.complete(NAME), Some(data));
}

#[test]
fn a_piece_lost_before_the_file_is_written() {
    let data = file(100, 15);
    let store = MemStore::default();
    assert!(send(&store, &data, 0, Some(70)).is_err());
    let d = data.clone();
    *store.before_complete.borrow_mut() = Some(Box::new(move |s: &MemStore| {
        s.remove(&piece_key(NAME, &d, 16));
    }));
    let e = send(&store, &data, 64, None).unwrap_err();
    assert!(e.transient);
    assert!(e.message.contains("no longer held"), "{e}");
    assert!(store.complete(NAME).is_none());
    // What is left stays, and the client goes on from there.
    assert_eq!(size(&store), 16);
    assert_eq!(send(&store, &data, 16, None), Ok(()));
    assert_eq!(store.complete(NAME), Some(data));
}

#[test]
fn concurrent_uploads_of_one_file() {
    let data = file(100, 16);
    // Another request finishes the file while this one is writing it.
    let store = MemStore::default();
    assert!(send(&store, &data, 0, Some(70)).is_err());
    let other = data.clone();
    *store.before_complete.borrow_mut() = Some(Box::new(move |s: &MemStore| {
        s.objects.borrow_mut().insert(upload_key(NAME), other);
    }));
    assert_eq!(send(&store, &data, 64, None), Ok(()));
    assert_eq!(store.complete(NAME), Some(data.clone()));

    // The same, with a file of another size: refused, nothing replaced.
    let store = MemStore::default();
    assert!(send(&store, &data, 0, Some(70)).is_err());
    *store.before_complete.borrow_mut() = Some(Box::new(|s: &MemStore| {
        s.objects
            .borrow_mut()
            .insert(upload_key(NAME), vec![1, 2, 3]);
    }));
    let e = send(&store, &data, 64, None).unwrap_err();
    assert!(!e.transient);
    assert_eq!(store.complete(NAME), Some(vec![1, 2, 3]));

    // Two requests from the start interleave piece by piece: they write
    // the same grid pieces with the same bytes, and either can finish.
    let store = MemStore::default();
    assert!(send(&store, &data, 0, Some(40)).is_err());
    assert!(send(&store, &data, 0, Some(80)).is_err());
    assert_eq!(size(&store), 80);
    assert_eq!(send(&store, &data, 32, None), Ok(()));
    assert_eq!(send(&store, &data, 80, None), Ok(()));
    assert_eq!(store.complete(NAME), Some(data));
}

#[test]
fn a_size_query_never_reports_a_finished_file_that_is_not_stored() {
    // Pieces end before the tail, so the size held is below the file size
    // however the cut falls.
    for len in [17usize, 32, 33, 64, 100] {
        let data = file(len, 17);
        let store = MemStore::default();
        assert!(send(&store, &data, 0, Some(len - 1)).is_err());
        let held = size(&store);
        assert!(held < len as u64, "{len}");
        assert_eq!(held, store.stored_from_zero(NAME, &data));
        assert_eq!(send(&store, &data, held, None), Ok(()));
        assert_eq!(store.complete(NAME), Some(data));
    }
}

/// Copies every stored piece of `from` to the keys of the file `to`, as if
/// a request for `to` had written them.
fn copy_pieces(store: &MemStore, from: &[u8], to: &[u8]) {
    let (a, b) = (md5_hex(from), md5_hex(to));
    let copies: Vec<(String, Vec<u8>)> = store
        .objects
        .borrow()
        .iter()
        .filter(|(k, _)| k.starts_with(FileKind::Partial.prefix()) && k.contains(&a))
        .map(|(k, v)| (k.replace(&a, &b), v.clone()))
        .collect();
    store.objects.borrow_mut().extend(copies);
}

#[test]
fn pieces_that_fail_the_check_are_dropped() {
    // Pieces stored under b's MD5 whose bytes are not b's: the store
    // refuses the finished file, and the pieces go so that the next attempt
    // does not fail the same way.
    let a = file(100, 18);
    let b = file(100, 19);
    let store = MemStore::default();
    assert!(send(&store, &a, 0, Some(50)).is_err());
    copy_pieces(&store, &a, &b);
    let keys: Vec<String> = store.pieces();
    for k in keys.iter().filter(|k| k.contains(&md5_hex(&a))) {
        store.remove(k);
    }
    assert_eq!(size(&store), 48);
    let e = send(&store, &b, 48, None).unwrap_err();
    assert!(e.transient);
    assert!(e.message.contains("from the start"), "{e}");
    assert!(store.complete(NAME).is_none());
    assert!(store.pieces().is_empty());
    assert_eq!(send(&store, &b, 0, None), Ok(()));
    assert_eq!(store.complete(NAME), Some(b));
}

/// What a stock client does with one file (`FILE_XFER::init_upload` and
/// `FILE_XFER_SET::poll`): ask for the size, count the upload as done when
/// the answer is at least the file's size, else send from that offset, and
/// ask again after an error.
#[derive(Debug, PartialEq, Eq)]
enum Client {
    /// The size answer was at least the file's size: no data was sent.
    DoneWithoutSending,
    Sent,
    GaveUp,
}

fn stock_client(store: &MemStore, data: &[u8]) -> Client {
    for _ in 0..3 {
        let held = size(store);
        if held >= data.len() as u64 {
            return Client::DoneWithoutSending;
        }
        if send(store, data, held, None).is_ok() {
            return Client::Sent;
        }
    }
    Client::GaveUp
}

#[test]
fn a_recomputed_file_never_mixes_with_an_earlier_attempt() {
    // An earlier attempt at 100 bytes was cut at 70 and left 64 bytes in
    // pieces. A task is run again and writes another file under the same
    // name, of any length. Nothing says the pieces are from an earlier
    // attempt: no result is known for the name, so the size query reports
    // them (see `a_resent_task_does_not_inherit_the_pieces_of_the_lost_one`
    // for the case the server can tell).
    let a = file(100, 20);
    for len in 17usize..=200 {
        let b = file(len, 21);
        let store = MemStore::default();
        assert!(send(&store, &a, 0, Some(70)).is_err());
        assert_eq!(size(&store), 64);

        let client = stock_client(&store, &b);
        if len > 64 {
            // The size query reports the old pieces, the request that
            // follows finds they are not this file's and drops them, and
            // the client's next query and request send the file whole. The
            // finished object is exactly the new file.
            assert_eq!(client, Client::Sent, "{len} bytes");
            assert_eq!(store.complete(NAME), Some(b), "{len} bytes");
            assert!(store.pieces().is_empty(), "{len} bytes");
        } else {
            // The size query cannot know the new file is shorter than the
            // bytes held, and the stock client then declares the upload
            // done without a request, as it does against the stock handler.
            // Nothing is stored under the finished name, so the result
            // fails validation instead of being taken for the old file.
            assert_eq!(client, Client::DoneWithoutSending, "{len} bytes");
            assert!(store.complete(NAME).is_none(), "{len} bytes");
        }
    }
}

/// Names an output template can make of one output: the plain form, text
/// after `<OUTFILE_n/>`, text before it, and both, with the `_r<digits>_`
/// of the generated part repeated in the decoration.
const NAMES: [&str; 5] = [
    NAME,
    "wu_1_0_r9_0_r1_log",
    "wu_1_0_r9_0.zip",
    "pre_wu_1_0_r9_0",
    "wu_1_0_r9_0_r1_pre_wu_1_0_r9_0_log",
];

fn send_as(
    name: &str,
    store: &MemStore,
    data: &[u8],
    offset: u64,
    cut: Option<usize>,
) -> Result<(), UploadError> {
    send_with(store, &header(name, data, offset as f64), data, cut, P)
}

fn size_as(store: &MemStore, name: &str) -> u64 {
    block_on(file_size(store, &store.db, name, P)).unwrap()
}

/// `stock_client` for a file of this name.
fn stock_client_as(store: &MemStore, name: &str, data: &[u8]) -> Client {
    for _ in 0..3 {
        let held = size_as(store, name);
        if held >= data.len() as u64 {
            return Client::DoneWithoutSending;
        }
        if send_as(name, store, data, held, None).is_ok() {
            return Client::Sent;
        }
    }
    Client::GaveUp
}

/// A first attempt at the file `name` was cut at 70 bytes and left 64 in
/// pieces.
fn lost_attempt(name: &str, a: &[u8]) -> MemStore {
    let store = MemStore::default();
    assert!(send_as(name, &store, a, 0, Some(70)).is_err());
    assert_eq!(size_as(&store, name), 64);
    store
}

/// Pieces of the file `name` as (epoch, start), sorted.
fn piece_epochs(store: &MemStore, name: &str) -> Vec<(u64, u64)> {
    let prefix = format!("{}{name}/", FileKind::Partial.prefix());
    let mut v: Vec<(u64, u64)> = store
        .pieces()
        .iter()
        .filter_map(|k| {
            let (epoch, _, start) = parse_piece_name(k.strip_prefix(&prefix)?)?;
            Some((epoch, start))
        })
        .collect();
    v.sort();
    v
}

#[test]
fn a_resent_task_does_not_inherit_the_pieces_of_the_lost_one() {
    // The client lost the task; the scheduler sent it again, which moves
    // the file to the next epoch. The recomputed file is of any length,
    // including one no longer than the 64 bytes held, and the client sends
    // it whole every time: the new file is stored and the size query never
    // reports it done before it is. The names are those an output template
    // can make, whatever their shape.
    let a = file(100, 30);
    for name in NAMES {
        for len in 1usize..=200 {
            let b = file(len, 31);
            let store = lost_attempt(name, &a);
            store.resend(name);
            assert_eq!(size_as(&store, name), 0, "{name} {len} bytes");
            assert_eq!(
                stock_client_as(&store, name, &b),
                Client::Sent,
                "{name} {len} bytes"
            );
            assert_eq!(
                store.objects.borrow().get(&upload_key(name)),
                Some(&b),
                "{name} {len} bytes"
            );
            // The old attempt's pieces are gone with the new file's.
            if len > P as usize {
                assert!(store.pieces().is_empty(), "{name} {len} bytes");
            }
        }

        // The same with a file of the very length that was lost.
        let b = file(100, 32);
        let store = lost_attempt(name, &a);
        store.resend(name);
        assert_eq!(stock_client_as(&store, name, &b), Client::Sent, "{name}");
        assert_eq!(store.objects.borrow().get(&upload_key(name)), Some(&b));
    }

    // A file the old attempt finished is still reported as it is.
    let store = MemStore::default();
    assert_eq!(send(&store, &a, 0, None), Ok(()));
    store.resend(NAME);
    assert_eq!(size(&store), a.len() as u64);

    // The epoch is kept for the file it names: another file of the same
    // result is not touched by it.
    let other = "wu_1_0_r9_1";
    let store = lost_attempt(other, &a);
    store.resend(NAME);
    assert_eq!(size_as(&store, other), 64);
}

/// The scheduler sends the task again when the request writes its piece
/// number `at`.
fn resend_at_piece(store: &MemStore, name: &str, at: usize) {
    let name = name.to_string();
    *store.before_piece.borrow_mut() = Some(Box::new(move |s: &MemStore, n: usize| {
        if n == at {
            s.resend(&name);
        }
    }));
}

#[test]
fn a_piece_of_the_lost_attempt_written_after_the_resend_is_never_counted() {
    // The reviewer's sequence. A request of the first attempt has read the
    // file's epoch when the scheduler sends the task again; its piece write
    // finishes afterwards (an upload that goes on after the client is gone).
    // R2 stores the piece after the resend, but under the epoch the request
    // captured. The recomputed file is no longer than the pieces it left.
    let a = file(100, 40);
    for name in NAMES {
        for len in [5usize, 15, 16, 17, 40, 100] {
            let store = MemStore::default();
            resend_at_piece(&store, name, 0);
            // The request goes on after the resend, cut at 70 bytes.
            assert!(send_as(name, &store, &a, 0, Some(70)).is_err());
            let epochs = piece_epochs(&store, name);
            assert_eq!(epochs, vec![(0, 0), (0, 16), (0, 32), (0, 48)], "{name}");
            *store.before_piece.borrow_mut() = None;
            let b = file(len, 41);
            assert_eq!(size_as(&store, name), 0, "{name} {len}");
            assert_eq!(
                stock_client_as(&store, name, &b),
                Client::Sent,
                "{name} {len}"
            );
            assert_eq!(
                store.objects.borrow().get(&upload_key(name)),
                Some(&b),
                "{name} {len}"
            );
        }
    }
}

#[test]
fn an_old_request_cannot_finish_the_file_after_the_resend() {
    // The request writes all its pieces and the task is sent again at its
    // last piece: it reads the epoch again before the finished write and
    // stops, with the file not stored.
    let a = file(100, 42);
    for name in NAMES {
        for at in [0usize, 5] {
            let store = MemStore::default();
            resend_at_piece(&store, name, at);
            let e = send_as(name, &store, &a, 0, None).unwrap_err();
            assert!(
                e.transient && e.message.contains("sent again"),
                "{name} {at}: {e}"
            );
            assert!(store.objects.borrow().get(&upload_key(name)).is_none());
            // Nothing the new attempt asks about is held, and the new
            // attempt sends a file of its own.
            assert_eq!(size_as(&store, name), 0);
            let b = file(100, 43);
            *store.before_piece.borrow_mut() = None;
            assert_eq!(stock_client_as(&store, name, &b), Client::Sent);
            assert_eq!(store.objects.borrow().get(&upload_key(name)), Some(&b));
        }
    }

    // A request that finds pieces of a later epoch than the one it read
    // belongs to a lost attempt: it stops, writes nothing and deletes
    // nothing of the later one's.
    let data = file(100, 44);
    let store = MemStore::default();
    let md5 = cb_core::upload::parse_md5(&md5_hex(&data)).unwrap();
    store
        .objects
        .borrow_mut()
        .insert(piece_key_md5(NAME, 1, &md5, 0), data[..16].to_vec());
    let before = store.objects.borrow().clone();
    let e = send(&store, &data, 0, None).unwrap_err();
    assert!(e.transient && e.message.contains("sent again"), "{e}");
    assert_eq!(*store.objects.borrow(), before);

    // A finished-file write that the resend races with, between the check
    // and the put, stores the old attempt's whole checksummed file; the new
    // attempt's request meets a stored file and is refused if its contents
    // differ, never mixed with it.
    let store = MemStore::default();
    let name = NAME.to_string();
    *store.before_complete.borrow_mut() = Some(Box::new(move |s: &MemStore| s.resend(&name)));
    assert_eq!(send(&store, &a, 0, None), Ok(()));
    let b = file(100, 45);
    let e = send(&store, &b, 0, None).unwrap_err();
    assert!(!e.transient, "{e}");
    assert_eq!(store.complete(NAME), Some(a));
}

#[test]
fn pieces_of_older_epochs_are_dropped_by_the_next_request() {
    let a = file(100, 46);
    let store = lost_attempt(NAME, &a);
    store.resend(NAME);
    // The new attempt's first request lists, deletes the old epoch's pieces
    // and writes its own.
    assert!(send(&store, &a, 0, Some(40)).is_err());
    assert_eq!(piece_epochs(&store, NAME), vec![(1, 0), (1, 16)]);
    assert_eq!(size(&store), 32);
}

#[test]
fn a_resumed_upload_of_the_same_attempt_keeps_its_pieces() {
    let data = file(100, 33);
    // A task sent again before the first request, pieces stored by two
    // requests. A resume needs no later second to count its pieces: the
    // epoch tells them from the lost attempt's however close the times.
    for name in NAMES {
        for resent in [false, true] {
            let store = MemStore::default();
            if resent {
                store.resend(name);
            }
            assert!(send_as(name, &store, &data, 0, Some(40)).is_err());
            assert_eq!(size_as(&store, name), 32);
            assert!(send_as(name, &store, &data, 32, Some(70)).is_err());
            assert_eq!(size_as(&store, name), 64);
            assert_eq!(stock_client_as(&store, name, &data), Client::Sent);
            assert_eq!(store.objects.borrow().get(&upload_key(name)), Some(&data));
        }
    }

    // A second resend moves the epoch again, and pieces of the first resend
    // are the lost attempt's then.
    let store = MemStore::default();
    store.resend(NAME);
    assert!(send(&store, &data, 0, Some(70)).is_err());
    assert_eq!(size(&store), 64);
    store.resend(NAME);
    assert_eq!(size(&store), 0);
    assert_eq!(stock_client(&store, &data), Client::Sent);
    assert_eq!(store.complete(NAME), Some(data));
    assert!(store.pieces().is_empty());
}

#[test]
fn a_name_the_scheduler_never_resent_keeps_its_pieces() {
    let a = file(100, 34);
    // No row: the first attempt, whatever the name looks like.
    for name in NAMES.into_iter().chain(["model.bin"]) {
        let store = lost_attempt(name, &a);
        assert_eq!(size_as(&store, name), 64);
        store.resend("another_file");
        assert_eq!(size_as(&store, name), 64);
        assert_eq!(
            piece_epochs(&store, name),
            vec![(0, 0), (0, 16), (0, 32), (0, 48)]
        );
    }
}

#[test]
fn database_reads_are_few_and_none_for_a_file_of_one_piece() {
    let a = file(100, 35);
    let store = MemStore::default();
    // Nothing held: a size query reads nothing.
    assert_eq!(size(&store), 0);
    assert_eq!(store.db_statements(), 0);
    // A file of one piece or less: one head and one put, no database read,
    // as before epochs.
    let small = file(10, 36);
    store.take_ops();
    assert_eq!(send_as("wu_1_0_r9_5", &store, &small, 0, None), Ok(()));
    assert_eq!(store.take_ops(), vec!["head", "put"]);
    assert_eq!(store.db_statements(), 0);
    // A larger file from offset 0: the epoch is read when the request starts
    // and again before the finished object is written.
    assert!(send(&store, &a, 0, Some(70)).is_err());
    assert_eq!(store.db_statements(), 1);
    assert_eq!(send(&store, &a, 64, None), Ok(()));
    assert_eq!(store.db_statements(), 3);
    // A size query with pieces held: one read; with a finished file: none.
    let store = MemStore::default();
    assert!(send(&store, &a, 0, Some(70)).is_err());
    let before = store.db_statements();
    store.take_ops();
    assert_eq!(size(&store), 64);
    assert_eq!(store.db_statements(), before + 1);
    assert_eq!(store.take_ops(), vec!["head", "list"]);
    assert_eq!(send(&store, &a, 64, None), Ok(()));
    let before = store.db_statements();
    assert_eq!(size(&store), 100);
    assert_eq!(store.db_statements(), before);
}

#[test]
fn a_recomputed_file_of_the_same_size_does_not_take_the_old_prefix() {
    // The reviewer's case: same name, same length, other contents.
    let a = file(100, 22);
    let b = file(100, 23);
    let store = MemStore::default();
    assert!(send(&store, &a, 0, Some(90)).is_err());
    assert_eq!(size(&store), 80);
    assert_eq!(stock_client(&store, &b), Client::Sent);
    assert_eq!(store.complete(NAME), Some(b));
    assert!(store.pieces().is_empty());
}

#[test]
fn pieces_of_two_files_are_not_reported() {
    let a = file(100, 24);
    let b = file(100, 25);
    let store = MemStore::default();
    assert!(send(&store, &a, 0, Some(70)).is_err());
    copy_pieces(&store, &a, &b);
    // Which file the client has is unknown.
    assert_eq!(size(&store), 0);
    // A request from the start drops the other file's pieces too.
    assert_eq!(send(&store, &b, 0, None), Ok(()));
    assert_eq!(store.complete(NAME), Some(b));
    assert!(store.pieces().is_empty());
}

#[test]
fn a_missing_or_malformed_checksum_reuses_nothing() {
    let bad = [
        "",
        "not-an-md5",
        "d41d8cd98f00b204e9800998ecf8427",
        "g41d8cd98f00b204e9800998ecf8427e",
        "d41d8cd98f00b204e9800998ecf8427e00",
    ];
    let a = file(100, 26);
    let b = file(100, 27);
    for md5 in bad {
        let with = |data: &[u8], offset: f64| {
            let mut h = header(NAME, data, offset);
            h.md5_cksum = md5.to_string();
            h
        };
        // From the start the file is stored whole, in one put, with no
        // piece: there is nothing for another request to pick up.
        let store = MemStore::default();
        assert_eq!(send_with(&store, &with(&a, 0.0), &a, None, P), Ok(()));
        let ops = store.take_ops();
        assert_eq!(MemStore::count(&ops, "put"), 1, "{md5:?}");
        assert_eq!(MemStore::count(&ops, "get"), 0, "{md5:?}");
        assert_eq!(store.complete(NAME), Some(a.clone()));
        assert!(store.pieces().is_empty());

        // A cut keeps nothing, so the client is told 0.
        let store = MemStore::default();
        assert!(send_with(&store, &with(&a, 0.0), &a, Some(70), P).is_err());
        assert!(store.pieces().is_empty());
        assert_eq!(size(&store), 0);

        // Pieces another request left are not combined with this file's
        // tail, whether the file is the same or another: the request is
        // refused, the pieces go, and the next size query says 0.
        for data in [&a, &b] {
            let store = MemStore::default();
            assert!(send(&store, &a, 0, Some(70)).is_err());
            assert_eq!(size(&store), 64);
            let e = send_with(&store, &with(data, 64.0), data, None, P).unwrap_err();
            assert!(e.transient, "{e}");
            assert!(store.complete(NAME).is_none(), "{md5:?}");
            assert!(store.pieces().is_empty(), "{md5:?}");
            assert_eq!(size(&store), 0);
            assert_eq!(
                send_with(&store, &with(data, 0.0), data, None, P),
                Ok(()),
                "{md5:?}"
            );
            assert_eq!(store.complete(NAME), Some(data.clone()));
        }
    }
}

#[test]
fn a_valid_checksum_in_capitals_resumes() {
    let data = file(100, 28);
    let store = MemStore::default();
    let mut h = header(NAME, &data, 0.0);
    h.md5_cksum = h.md5_cksum.to_uppercase();
    assert!(send_with(&store, &h, &data, Some(70), P).is_err());
    assert_eq!(size(&store), 64);
    h.offset = 64.0;
    assert_eq!(send_with(&store, &h, &data, None, P), Ok(()));
    assert_eq!(store.complete(NAME), Some(data));
}
