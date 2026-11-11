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
    check_header, file_size, receive, CompleteWrite, PieceWrite, UploadStore, PIECE_BYTES,
};
use cb_core::{CoreResult, FileKind};
use cb_crypto::md5_hex;
use cb_proto::upload::{UploadError, UploadHeader};
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

#[derive(Default)]
struct MemStore {
    objects: RefCell<BTreeMap<String, Vec<u8>>>,
    ops: RefCell<Vec<&'static str>>,
    /// Runs when the next finished-file write starts.
    before_complete: RefCell<Option<Hook>>,
}

fn upload_key(name: &str) -> String {
    format!("{}{name}", FileKind::Upload.prefix())
}

fn piece_key(name: &str, start: u64) -> String {
    format!("{}{name}/{start}", FileKind::Partial.prefix())
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

    /// Bytes from 0 that stored pieces really hold.
    fn stored_from_zero(&self, name: &str) -> u64 {
        let mut at = 0;
        while let Some(p) = self.objects.borrow().get(&piece_key(name, at)) {
            at += p.len() as u64;
        }
        at
    }

    fn remove(&self, key: &str) {
        self.objects.borrow_mut().remove(key);
    }
}

#[async_trait(?Send)]
impl UploadStore for MemStore {
    type Body = MemBody;

    async fn complete_size(&self, name: &str) -> CoreResult<Option<u64>> {
        self.op("head");
        Ok(self.complete(name).map(|d| d.len() as u64))
    }

    async fn has_piece(&self, name: &str, start: u64) -> CoreResult<bool> {
        self.op("head");
        Ok(self.objects.borrow().contains_key(&piece_key(name, start)))
    }

    async fn list_pieces(&self, name: &str) -> CoreResult<Vec<(u64, u64)>> {
        self.op("list");
        let prefix = format!("{}{name}/", FileKind::Partial.prefix());
        Ok(self
            .objects
            .borrow()
            .iter()
            .filter_map(|(k, v)| {
                let start = k.strip_prefix(&prefix)?.parse().ok()?;
                Some((start, v.len() as u64))
            })
            .collect())
    }

    async fn skip(&self, body: &MemBody, n: u64) -> CoreResult<bool> {
        Ok(body.read(n).is_some())
    }

    async fn put_piece(
        &self,
        name: &str,
        start: u64,
        body: &MemBody,
        len: u64,
    ) -> CoreResult<PieceWrite> {
        self.op("put");
        let Some(bytes) = body.read(len) else {
            return Ok(PieceWrite::BodyEnded);
        };
        self.objects
            .borrow_mut()
            .insert(piece_key(name, start), bytes);
        Ok(PieceWrite::Stored)
    }

    async fn put_complete(
        &self,
        name: &str,
        starts: &[u64],
        piece: u64,
        body: &MemBody,
        tail: u64,
        md5: Option<[u8; 16]>,
    ) -> CoreResult<CompleteWrite> {
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
            match self.objects.borrow().get(&piece_key(name, *s)) {
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

    async fn delete_pieces(&self, name: &str, starts: &[u64]) -> CoreResult<()> {
        self.op("delete");
        for s in starts {
            self.remove(&piece_key(name, *s));
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
    block_on(receive(store, h, &body, piece)).unwrap()
}

fn send(store: &MemStore, data: &[u8], offset: u64, cut: Option<usize>) -> Result<(), UploadError> {
    send_with(store, &header(NAME, data, offset as f64), data, cut, P)
}

fn size(store: &MemStore) -> u64 {
    block_on(file_size(store, NAME, P)).unwrap()
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

    // A size query for a file never seen: two heads.
    let store = MemStore::default();
    assert_eq!(size(&store), 0);
    assert_eq!(store.take_ops(), ["head", "head"]);
}

#[test]
fn large_upload_writes_pieces_then_one_object() {
    let store = MemStore::default();
    let data = file(100, 3);
    assert_eq!(send(&store, &data, 0, None), Ok(()));
    let ops = store.take_ops();
    // Head, six pieces, the finished file (reading the six back), one
    // delete of the pieces.
    assert_eq!(MemStore::count(&ops, "head"), 1);
    assert_eq!(MemStore::count(&ops, "put"), 7);
    assert_eq!(MemStore::count(&ops, "get"), 6);
    assert_eq!(MemStore::count(&ops, "delete"), 1);
    assert_eq!(MemStore::count(&ops, "list"), 0);
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
        assert!(held <= store.stored_from_zero(NAME));
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
        let e = block_on(receive(&store, &h, &body, P)).unwrap().unwrap_err();
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
    // Other contents of the same size do not replace it.
    let other = file(100, 10);
    assert_eq!(send(&store, &other, 0, None), Ok(()));
    assert_eq!(store.complete(NAME), Some(data.clone()));

    // A longer file under the same name, resumed past the old end.
    let longer = file(150, 9);
    let e = send(&store, &longer, 100, None).unwrap_err();
    assert_eq!(
        e,
        UploadError::permanent(format!("file {NAME} exists with another size"))
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
    // pieces an earlier, longer attempt left.
    let store = MemStore::default();
    assert!(send(&store, &data, 0, Some(99)).is_err());
    assert_eq!(size(&store), 96);
    let short = data[..40].to_vec();
    assert_eq!(send(&store, &short, 32, None), Ok(()));
    assert_eq!(store.complete(NAME), Some(short));
}

#[test]
fn contents_that_do_not_match_start_over() {
    let a = file(100, 12);
    let b = file(100, 13);
    let store = MemStore::default();
    assert!(send(&store, &a, 0, Some(50)).is_err());
    assert_eq!(size(&store), 48);
    // The tail and MD5 of another file: the store refuses the result, and
    // the pieces go so that the next attempt does not fail the same way.
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
    store.remove(&piece_key(NAME, 0));
    assert_eq!(size(&store), 0);
    assert!(send(&store, &data, 64, None).unwrap_err().transient);
    assert_eq!(send(&store, &data, 0, None), Ok(()));
    assert_eq!(store.complete(NAME), Some(data.clone()));
    assert!(store.pieces().is_empty());

    // A gap ends what is held.
    let store = MemStore::default();
    assert!(send(&store, &data, 0, Some(70)).is_err());
    store.remove(&piece_key(NAME, 32));
    assert_eq!(size(&store), 32);
    assert_eq!(send(&store, &data, 32, None), Ok(()));
    assert_eq!(store.complete(NAME), Some(data));
}

#[test]
fn a_piece_lost_before_the_file_is_written() {
    let data = file(100, 15);
    let store = MemStore::default();
    assert!(send(&store, &data, 0, Some(70)).is_err());
    *store.before_complete.borrow_mut() = Some(Box::new(|s: &MemStore| {
        s.remove(&piece_key(NAME, 16));
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
        s.objects.borrow_mut().insert(upload_key(NAME), vec![1, 2, 3]);
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
        assert_eq!(held, store.stored_from_zero(NAME));
        assert_eq!(send(&store, &data, held, None), Ok(()));
        assert_eq!(store.complete(NAME), Some(data));
    }
}
