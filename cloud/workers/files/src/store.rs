//! R2 behind `cb_core::upload::UploadStore`. The policy is in
//! `crates/cb-core/src/upload.rs`; this file moves bytes.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::pin::Pin;
use std::rc::Rc;

use async_trait::async_trait;
use cb_core::upload::{piece_name, CompleteWrite, PieceWrite, UploadStore};
use cb_core::{CoreError, CoreResult, FileKind};
use cb_worker::object_key;
use futures_util::{Stream, StreamExt};
use worker::{console_error, Bucket, ByteStream, Conditional, Error, FixedLengthStream, Result};

struct BodyState {
    /// Bytes read from the request but not yet used.
    pending: Vec<u8>,
    /// The rest of the request body; `None` while a read is in flight.
    stream: Option<ByteStream>,
    /// The body ended or failed before a read had what it asked for.
    failed: bool,
}

/// The unread rest of a request body: what was read along with the header,
/// then the stream. Clones share the same position.
#[derive(Clone)]
pub struct Body(Rc<RefCell<BodyState>>);

impl Body {
    pub fn new(prefix: Vec<u8>, stream: ByteStream) -> Self {
        Body(Rc::new(RefCell::new(BodyState {
            pending: prefix,
            stream: Some(stream),
            failed: false,
        })))
    }

    fn failed(&self) -> bool {
        self.0.borrow().failed
    }

    fn fail(&self) {
        self.0.borrow_mut().failed = true;
    }

    /// The next bytes of the body, at most `max` (> 0) of them; `None` at
    /// its end.
    async fn next(&self, max: u64) -> Option<Result<Vec<u8>>> {
        let max = usize::try_from(max).unwrap_or(usize::MAX);
        {
            let mut st = self.0.borrow_mut();
            if !st.pending.is_empty() {
                let n = st.pending.len().min(max);
                let rest = st.pending.split_off(n);
                return Some(Ok(std::mem::replace(&mut st.pending, rest)));
            }
        }
        let mut stream = self.0.borrow_mut().stream.take()?;
        let chunk = loop {
            match stream.next().await {
                Some(Ok(c)) if c.is_empty() => continue,
                other => break other,
            }
        };
        self.0.borrow_mut().stream = Some(stream);
        match chunk? {
            Ok(mut c) => {
                if c.len() > max {
                    self.0.borrow_mut().pending = c.split_off(max);
                }
                Some(Ok(c))
            }
            Err(e) => Some(Err(e)),
        }
    }

    /// The next `len` bytes of the body as a stream. If the body ends
    /// sooner, the stream fails, so a put of it stores nothing.
    fn take(&self, len: u64) -> impl Stream<Item = Result<Vec<u8>>> + 'static {
        futures_util::stream::unfold((self.clone(), len), |(body, left)| async move {
            if left == 0 {
                return None;
            }
            let item = match body.next(left).await {
                Some(Ok(c)) => {
                    let left = left - c.len() as u64;
                    return Some((Ok(c), (body, left)));
                }
                Some(Err(e)) => Err(e),
                None => Err(Error::RustError("the request body ended early".into())),
            };
            body.fail();
            Some((item, (body, 0)))
        })
    }
}

/// The finished file: stored pieces read back in order, then the tail
/// from the request.
struct Compose {
    bucket: Bucket,
    keys: VecDeque<String>,
    piece: u64,
    current: Option<ByteStream>,
    tail: Pin<Box<dyn Stream<Item = Result<Vec<u8>>>>>,
    missing: Rc<Cell<bool>>,
    done: bool,
}

async fn open_piece(bucket: &Bucket, key: &str, piece: u64) -> Result<ByteStream> {
    let object = bucket
        .get(key)
        .execute()
        .await?
        .ok_or_else(|| Error::RustError(format!("{key} is missing")))?;
    if object.size() != piece {
        return Err(Error::RustError(format!("{key} has the wrong size")));
    }
    object
        .body()
        .ok_or_else(|| Error::RustError(format!("{key} has no body")))?
        .stream()
}

fn compose_stream(st: Compose) -> impl Stream<Item = Result<Vec<u8>>> + 'static {
    futures_util::stream::unfold(st, |mut st| async move {
        if st.done {
            return None;
        }
        loop {
            if let Some(current) = st.current.as_mut() {
                match current.next().await {
                    Some(Ok(c)) => return Some((Ok(c), st)),
                    Some(Err(e)) => {
                        st.missing.set(true);
                        st.done = true;
                        return Some((Err(e), st));
                    }
                    None => st.current = None,
                }
            }
            let Some(key) = st.keys.pop_front() else {
                break;
            };
            match open_piece(&st.bucket, &key, st.piece).await {
                Ok(s) => st.current = Some(s),
                Err(e) => {
                    st.missing.set(true);
                    st.done = true;
                    return Some((Err(e), st));
                }
            }
        }
        let item = st.tail.next().await?;
        if item.is_err() {
            st.done = true;
        }
        Some((item, st))
    })
}

fn not_exists() -> Conditional {
    Conditional {
        etag_matches: None,
        etag_does_not_match: Some("*".into()),
        uploaded_before: None,
        uploaded_after: None,
    }
}

fn files_err(e: Error) -> CoreError {
    CoreError::Files(e.to_string())
}

fn partial_key(name: &str, start: u64) -> String {
    object_key(FileKind::Partial, &piece_name(name, start))
}

pub struct R2Uploads(pub Bucket);

#[async_trait(?Send)]
impl UploadStore for R2Uploads {
    type Body = Body;

    async fn complete_size(&self, name: &str) -> CoreResult<Option<u64>> {
        let head = self
            .0
            .head(object_key(FileKind::Upload, name))
            .await
            .map_err(files_err)?;
        Ok(head.map(|o| o.size()))
    }

    async fn has_piece(&self, name: &str, start: u64) -> CoreResult<bool> {
        let head = self
            .0
            .head(partial_key(name, start))
            .await
            .map_err(files_err)?;
        Ok(head.is_some())
    }

    async fn list_pieces(&self, name: &str) -> CoreResult<Vec<(u64, u64)>> {
        let prefix = object_key(FileKind::Partial, &format!("{name}/"));
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut list = self.0.list().prefix(prefix.clone());
            if let Some(c) = cursor.take() {
                list = list.cursor(c);
            }
            let page = list.execute().await.map_err(files_err)?;
            for o in page.objects() {
                if let Some(Ok(start)) = o.key().strip_prefix(&prefix).map(str::parse::<u64>) {
                    out.push((start, o.size()));
                }
            }
            match page.cursor() {
                Some(c) if page.truncated() => cursor = Some(c),
                _ => return Ok(out),
            }
        }
    }

    async fn skip(&self, body: &Body, n: u64) -> CoreResult<bool> {
        let mut left = n;
        while left > 0 {
            match body.next(left).await {
                Some(Ok(c)) => left -= c.len() as u64,
                _ => {
                    body.fail();
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    async fn put_piece(
        &self,
        name: &str,
        start: u64,
        body: &Body,
        len: u64,
    ) -> CoreResult<PieceWrite> {
        let data = FixedLengthStream::wrap(body.take(len), len);
        match self.0.put(partial_key(name, start), data).execute().await {
            Ok(_) => Ok(PieceWrite::Stored),
            Err(_) if body.failed() => Ok(PieceWrite::BodyEnded),
            Err(e) => Err(files_err(e)),
        }
    }

    async fn put_complete(
        &self,
        name: &str,
        starts: &[u64],
        piece: u64,
        body: &Body,
        tail: u64,
        md5: Option<[u8; 16]>,
    ) -> CoreResult<CompleteWrite> {
        let missing = Rc::new(Cell::new(false));
        let compose = Compose {
            bucket: self.0.clone(),
            keys: starts.iter().map(|s| partial_key(name, *s)).collect(),
            piece,
            current: None,
            tail: Box::pin(body.take(tail)),
            missing: missing.clone(),
            done: false,
        };
        let total = starts.len() as u64 * piece + tail;
        let data = FixedLengthStream::wrap(compose_stream(compose), total);
        // The check for an existing file and this write are not one step.
        // The condition keeps a slow upload that started before another one
        // finished from replacing the stored object.
        let mut put = self
            .0
            .put(object_key(FileKind::Upload, name), data)
            .only_if(not_exists());
        if let Some(md5) = md5 {
            put = put.md5(md5.to_vec());
        }
        Ok(match put.execute().await {
            Ok(Some(_)) => CompleteWrite::Stored,
            Ok(None) => CompleteWrite::Exists,
            Err(_) if body.failed() => CompleteWrite::BodyEnded,
            Err(_) if missing.get() => CompleteWrite::PieceMissing,
            Err(e) => {
                console_error!("upload {name}: {e}");
                CompleteWrite::Rejected
            }
        })
    }

    async fn delete_pieces(&self, name: &str, starts: &[u64]) -> CoreResult<()> {
        for chunk in starts.chunks(1000) {
            let keys: Vec<String> = chunk.iter().map(|s| partial_key(name, *s)).collect();
            self.0.delete_multiple(keys).await.map_err(files_err)?;
        }
        Ok(())
    }
}
