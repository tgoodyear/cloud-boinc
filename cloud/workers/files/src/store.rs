//! R2 behind `cb_core::upload::UploadStore`. The policy is in
//! `crates/cb-core/src/upload.rs`; this file moves bytes.
//!
//! File bytes never enter wasm: reading the request body, cutting it into
//! pieces and joining the pieces with the tail happen in `src/upload.js`,
//! on the platform's own streams. Only the header is copied in, to be
//! parsed.

use async_trait::async_trait;
use cb_core::upload::{
    parse_piece_name, piece_name, CompleteWrite, PieceWrite, StoredFile, StoredPiece, UploadStore,
};
use cb_core::{CoreError, CoreResult, FileKind};
use cb_worker::object_key;
use js_sys::{Array, Promise, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::ReadableStream;
use worker::{console_error, Bucket, Conditional, Error, Result};

#[wasm_bindgen(module = "/src/upload.js")]
extern "C" {
    type Splitter;

    #[wasm_bindgen(constructor)]
    fn new(stream: Option<ReadableStream>) -> Splitter;
    #[wasm_bindgen(method, getter)]
    fn failed(this: &Splitter) -> bool;
    #[wasm_bindgen(method, js_name = readMore)]
    fn read_more(this: &Splitter) -> Promise;
    #[wasm_bindgen(method)]
    fn consume(this: &Splitter, n: f64);
    #[wasm_bindgen(method)]
    fn skip(this: &Splitter, n: f64) -> Promise;
    #[wasm_bindgen(method)]
    fn take(this: &Splitter, n: f64) -> ReadableStream;
    #[wasm_bindgen(method)]
    fn compose(this: &Splitter, bucket: &JsValue, keys: Array, piece: f64, tail: f64) -> Composed;

    type Composed;

    #[wasm_bindgen(method, getter)]
    fn readable(this: &Composed) -> ReadableStream;
    #[wasm_bindgen(method, getter)]
    fn missing(this: &Composed) -> bool;
}

/// A request body. The header is read from it with [`Body::read_more`];
/// after [`Body::start_data`], the rest is the file data.
pub struct Body(Splitter);

impl Body {
    pub fn new(stream: Option<ReadableStream>) -> Self {
        Body(Splitter::new(stream))
    }

    /// The next chunk of the body, for the header parser; `None` at its
    /// end. The chunk also stays unread on the JavaScript side.
    pub async fn read_more(&self) -> Result<Option<Vec<u8>>> {
        let chunk = JsFuture::from(self.0.read_more())
            .await
            .map_err(Error::from)?;
        Ok((!chunk.is_undefined()).then(|| Uint8Array::new(&chunk).to_vec()))
    }

    /// Drops the first `header_bytes` bytes read so far; what is left is
    /// the file data.
    pub fn start_data(&self, header_bytes: usize) {
        self.0.consume(header_bytes as f64);
    }

    /// The body ended or failed before a read had what it asked for.
    fn failed(&self) -> bool {
        self.0.failed()
    }
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

fn partial_key(name: &str, md5: &[u8; 16], start: u64) -> String {
    object_key(FileKind::Partial, &piece_name(name, md5, start))
}

pub struct R2Uploads(pub Bucket);

#[async_trait(?Send)]
impl UploadStore for R2Uploads {
    type Body = Body;

    async fn complete(&self, name: &str) -> CoreResult<Option<StoredFile>> {
        let head = self
            .0
            .head(object_key(FileKind::Upload, name))
            .await
            .map_err(files_err)?;
        Ok(head.map(|o| StoredFile {
            size: o.size(),
            md5: o.checksum().md5.and_then(|m| m.try_into().ok()),
        }))
    }

    async fn list_pieces(&self, name: &str) -> CoreResult<Vec<StoredPiece>> {
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
                if let Some((md5, start)) = o.key().strip_prefix(&prefix).and_then(parse_piece_name)
                {
                    out.push(StoredPiece {
                        md5,
                        start,
                        size: o.size(),
                    });
                }
            }
            match page.cursor() {
                Some(c) if page.truncated() => cursor = Some(c),
                _ => return Ok(out),
            }
        }
    }

    async fn skip(&self, body: &Body, n: u64) -> CoreResult<bool> {
        let done = JsFuture::from(body.0.skip(n as f64))
            .await
            .map_err(|e| files_err(e.into()))?;
        Ok(done.as_bool() == Some(true))
    }

    async fn put_piece(
        &self,
        name: &str,
        md5: &[u8; 16],
        start: u64,
        body: &Body,
        len: u64,
    ) -> CoreResult<PieceWrite> {
        let data = body.0.take(len as f64);
        match self
            .0
            .put(partial_key(name, md5, start), data)
            .execute()
            .await
        {
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
        // Pieces are only composed under the MD5 they were stored for, and
        // `receive` passes starts only with one.
        let keys: Array = match md5 {
            Some(m) => starts
                .iter()
                .map(|s| JsValue::from(partial_key(name, &m, *s)))
                .collect(),
            None => Array::new(),
        };
        let composed = body
            .0
            .compose(self.0.as_ref(), keys, piece as f64, tail as f64);
        // The check for an existing file and this write are not one step.
        // The condition keeps a slow upload that started before another one
        // finished from replacing the stored object.
        let mut put = self
            .0
            .put(object_key(FileKind::Upload, name), composed.readable())
            .only_if(not_exists());
        if let Some(md5) = md5 {
            put = put.md5(md5.to_vec());
        }
        Ok(match put.execute().await {
            Ok(Some(_)) => CompleteWrite::Stored,
            Ok(None) => CompleteWrite::Exists,
            Err(_) if body.failed() => CompleteWrite::BodyEnded,
            Err(_) if composed.missing() => CompleteWrite::PieceMissing,
            Err(e) => {
                console_error!("upload {name}: {e}");
                CompleteWrite::Rejected
            }
        })
    }

    async fn delete_pieces(&self, name: &str, pieces: &[StoredPiece]) -> CoreResult<()> {
        for chunk in pieces.chunks(1000) {
            let keys: Vec<String> = chunk
                .iter()
                .map(|p| partial_key(name, &p.md5, p.start))
                .collect();
            self.0.delete_multiple(keys).await.map_err(files_err)?;
        }
        Ok(())
    }
}
