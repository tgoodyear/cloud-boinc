//! File transfer: `GET /download/<name>` from the bucket and `POST /upload/file_upload_handler`,
//! the `file_upload_handler` protocol, with resume (`cb_core::upload`). This Worker holds no
//! private key.

mod store;

use cb_core::upload::{self, is_flat_name, PIECE_BYTES};
use cb_core::FileKind;
use cb_proto::upload::{
    check_get_file_size_name, incomplete_error, parse_request, reply_error, reply_file_size,
    reply_success, UploadError, UploadRequest, MAX_HEADER_BYTES,
};
use cb_proto::ProtoError;
use cb_worker::{db, object_key, project, text, xml, BUCKET};
use store::{Body, R2Uploads};
use worker::{
    console_error, console_log, event, Bucket, Context, Env, Headers, Method, Object, Range,
    Request, Response, Result,
};

/// A client treats a `<url>` in a `<file_info>` as an upload URL only when it
/// contains `file_upload_handler`, so the path keeps the stock handler's name.
const UPLOAD_PATH: &str = "/upload/file_upload_handler";

/// One `bytes=` range of a `Range` header, as first and last byte.
/// `None`: no usable range, serve the whole object. `Err`: not satisfiable.
fn parse_range(header: &str, size: u64) -> std::result::Result<Option<(u64, u64)>, ()> {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((first, last)) = spec.split_once('-') else {
        return Ok(None);
    };
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        let Ok(n) = last.parse::<u64>() else {
            return Ok(None);
        };
        if n == 0 || size == 0 {
            return Err(());
        }
        return Ok(Some((size.saturating_sub(n), size - 1)));
    }
    let Ok(start) = first.parse::<u64>() else {
        return Ok(None);
    };
    if start >= size {
        return Err(());
    }
    let end = match last.parse::<u64>() {
        Ok(e) if e < start => return Ok(None),
        Ok(e) => e.min(size - 1),
        Err(_) if last.is_empty() => size - 1,
        Err(_) => return Ok(None),
    };
    Ok(Some((start, end)))
}

fn object_headers(object: &Object) -> Result<Headers> {
    let headers = Headers::new();
    headers.set("Content-Type", "application/octet-stream")?;
    headers.set("ETag", &object.http_etag())?;
    headers.set("Accept-Ranges", "bytes")?;
    headers.set("Cache-Control", "public, max-age=300")?;
    Ok(headers)
}

async fn download(req: &Request, bucket: &Bucket, name: &str) -> Result<Response> {
    if !is_flat_name(name) {
        return text(404, "not found\n");
    }
    let key = object_key(FileKind::Download, name);
    let range_header = req.headers().get("Range")?;
    let head_only = req.method() == Method::Head;
    if head_only || range_header.is_some() {
        let Some(head) = bucket.head(&key).await? else {
            return text(404, "not found\n");
        };
        let size = head.size();
        let headers = object_headers(&head)?;
        let range = match range_header.as_deref().map(|h| parse_range(h, size)) {
            Some(Err(())) => {
                headers.set("Content-Range", &format!("bytes */{size}"))?;
                return Ok(Response::builder()
                    .with_status(416)
                    .with_headers(headers)
                    .empty());
            }
            Some(Ok(r)) => r,
            None => None,
        };
        if let Some((start, end)) = range {
            headers.set("Content-Range", &format!("bytes {start}-{end}/{size}"))?;
            if head_only {
                headers.set("Content-Length", &(end - start + 1).to_string())?;
                return Ok(Response::builder()
                    .with_status(206)
                    .with_headers(headers)
                    .empty());
            }
            let Some(object) = bucket
                .get(&key)
                .range(Range::OffsetWithLength {
                    offset: start,
                    length: end - start + 1,
                })
                .execute()
                .await?
            else {
                return text(404, "not found\n");
            };
            let Some(body) = object.body() else {
                return text(404, "not found\n");
            };
            return Ok(Response::builder()
                .with_status(206)
                .with_headers(headers)
                .body(body.response_body()?));
        }
        if head_only {
            headers.set("Content-Length", &size.to_string())?;
            return Ok(Response::builder().with_headers(headers).empty());
        }
    }
    let Some(object) = bucket.get(&key).execute().await? else {
        return text(404, "not found\n");
    };
    let headers = object_headers(&object)?;
    let Some(body) = object.body() else {
        return text(404, "not found\n");
    };
    Ok(Response::builder()
        .with_headers(headers)
        .body(body.response_body()?))
}

fn refuse(e: UploadError) -> Result<Response> {
    xml(e.reply())
}

async fn upload(req: Request, env: Env) -> Result<Response> {
    let body = Body::new(req.inner().body());
    let mut buf: Vec<u8> = Vec::new();
    let request = loop {
        match parse_request(&buf) {
            Ok(r) => break r,
            Err(ProtoError::Incomplete) if buf.len() <= MAX_HEADER_BYTES => {
                match body.read_more().await? {
                    Some(chunk) => buf.extend_from_slice(&chunk),
                    None => return refuse(incomplete_error(&buf)),
                }
            }
            Err(_) => return refuse(UploadError::permanent("the request header is too long")),
        }
    };
    let store = R2Uploads(env.bucket(BUCKET)?);
    match request {
        UploadRequest::GetFileSize { file_name, .. } => {
            if let Err(e) = check_get_file_size_name(&file_name) {
                return refuse(e);
            }
            if !is_flat_name(&file_name) {
                return refuse(UploadError::permanent("invalid filename"));
            }
            match upload::file_size(&store, &file_name, PIECE_BYTES).await {
                Ok(size) => xml(reply_file_size(size)),
                Err(e) => {
                    console_error!("file size {file_name}: {e}");
                    refuse(UploadError::transient("cannot stat file"))
                }
            }
        }
        UploadRequest::FileUpload(header) => {
            // A header that arrived whole in one large chunk is held to the
            // same limit as one read piece by piece.
            if header.data_offset > MAX_HEADER_BYTES {
                return refuse(UploadError::permanent("the request header is too long"));
            }
            let db = db(&env)?;
            let project = match project(&db).await {
                Ok(p) => p,
                Err(e) => {
                    console_error!("settings: {e}");
                    return xml(reply_error(true, "the project is temporarily unavailable"));
                }
            };
            if let Err(e) = upload::check_header(
                &header,
                project.config.ignore_upload_certificates,
                project.upload_public_key.as_ref(),
                project.config.max_upload_nbytes,
            ) {
                return refuse(e);
            }
            body.start_data(header.data_offset.min(buf.len()));
            match upload::receive(&store, &header, &body, PIECE_BYTES).await {
                Ok(Ok(())) => {
                    console_log!(
                        "upload {} {} bytes from offset {}",
                        header.name,
                        header.nbytes,
                        header.offset
                    );
                    xml(reply_success())
                }
                Ok(Err(e)) => {
                    console_log!("upload {}: {}", header.name, e.message);
                    refuse(e)
                }
                Err(e) => {
                    console_error!("upload {}: {e}", header.name);
                    refuse(UploadError::transient("the file could not be stored"))
                }
            }
        }
    }
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, ctx: Context) -> Result<Response> {
    let path = req.path();
    match req.method() {
        Method::Get | Method::Head => match path.strip_prefix("/download/") {
            Some(name) => download(&req, &env.bucket(BUCKET)?, name).await,
            None => text(404, "not found\n"),
        },
        // The upload runs under `wait_until`: if the client drops the
        // connection while the body streams in, the runtime cancels the
        // request's own work but lets this finish, so a piece whose bytes
        // all arrived is still stored. This only applies when the zone's
        // `request_body_buffering` is `none` for the upload path; by default
        // the edge holds the whole body first, so a dropped request never
        // reaches the Worker.
        Method::Post if path == UPLOAD_PATH => {
            let (tx, rx) = futures_channel::oneshot::channel();
            ctx.wait_until(async move {
                let _ = tx.send(upload(req, env).await);
            });
            rx.await
                .unwrap_or_else(|_| Err(worker::Error::RustError("the upload was dropped".into())))
        }
        _ => text(405, "method not allowed\n"),
    }
}
