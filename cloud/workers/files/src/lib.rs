//! File transfer: `GET /download/<name>` from the bucket and `POST /upload/file_upload_handler`,
//! the `file_upload_handler` protocol. This Worker holds no private key.

use cb_core::FileKind;
use cb_crypto::verify_upload_certificate;
use cb_proto::upload::{
    check_get_file_size_name, incomplete_error, is_valid_filename, parse_request, reply_error,
    reply_file_size, reply_success, UploadError, UploadHeader, UploadRequest, MAX_HEADER_BYTES,
};
use cb_proto::ProtoError;
use cb_worker::{db, object_key, project, text, xml, BUCKET};
use futures_util::StreamExt;
use worker::{
    console_error, console_log, event, Bucket, ByteStream, Conditional, Context, Env,
    FixedLengthStream, Headers, Method, Object, Range, Request, Response, Result,
};

/// A client treats a `<url>` in a `<file_info>` as an upload URL only when it
/// contains `file_upload_handler`, so the path keeps the stock handler's name.
const UPLOAD_PATH: &str = "/upload/file_upload_handler";

fn is_flat_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && is_valid_filename(name) && !name.contains('/')
}

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

/// The file data of an upload: what was read along with the header, then
/// the rest of the request body, cut at `nbytes`.
fn data_stream(prefix: Vec<u8>, body: ByteStream, nbytes: u64) -> FixedLengthStream {
    let stream = futures_util::stream::unfold(
        (Some(prefix), body, nbytes),
        |(prefix, mut body, remaining)| async move {
            if remaining == 0 {
                return None;
            }
            let chunk = match prefix {
                Some(p) if !p.is_empty() => Ok(p),
                _ => loop {
                    match body.next().await? {
                        Ok(c) if c.is_empty() => continue,
                        other => break other,
                    }
                },
            };
            match chunk {
                Ok(mut c) => {
                    if c.len() as u64 > remaining {
                        c.truncate(remaining as usize);
                    }
                    let left = remaining - c.len() as u64;
                    Some((Ok(c), (None, body, left)))
                }
                Err(e) => Some((Err(e), (None, body, 0))),
            }
        },
    );
    FixedLengthStream::wrap(stream, nbytes)
}

async fn store(
    bucket: &Bucket,
    header: &UploadHeader,
    prefix: Vec<u8>,
    body: ByteStream,
) -> Result<Response> {
    let key = object_key(FileKind::Upload, &header.name);
    let nbytes = header.nbytes as u64;
    let offset = header.offset as u64;
    let existing = bucket.head(&key).await?;
    if let Some(object) = &existing {
        // A stored output is never replaced: the upload was complete, and
        // the result may have been validated against it since.
        if object.size() == nbytes {
            return xml(reply_success());
        }
        return refuse(UploadError::permanent(format!(
            "file {} exists with another size",
            header.name
        )));
    }
    if offset != 0 {
        return refuse(UploadError::transient(
            "partial uploads are not kept; send the file from the start",
        ));
    }
    // The check above and the write are not one step. The condition keeps
    // a slow upload that started before another one finished from replacing
    // the stored object.
    let mut put = bucket
        .put(&key, data_stream(prefix, body, nbytes))
        .only_if(Conditional {
            etag_matches: None,
            etag_does_not_match: Some("*".into()),
            uploaded_before: None,
            uploaded_after: None,
        });
    if let Ok(md5) = hex::decode(header.md5_cksum.trim()) {
        if md5.len() == 16 {
            put = put.md5(md5);
        }
    }
    match put.execute().await {
        Ok(Some(_)) => {
            console_log!("upload {} {} bytes", header.name, nbytes);
            xml(reply_success())
        }
        // Another request stored the file first. A retry of the same
        // upload is answered as the check above answers it.
        Ok(None) => match bucket.head(&key).await? {
            Some(object) if object.size() == nbytes => xml(reply_success()),
            _ => refuse(UploadError::permanent(format!(
                "file {} exists with another size",
                header.name
            ))),
        },
        Err(e) => {
            console_error!("upload {}: {e}", header.name);
            refuse(UploadError::transient("the file could not be stored"))
        }
    }
}

async fn upload(mut req: Request, env: &Env) -> Result<Response> {
    let mut body = req.stream()?;
    let mut buf: Vec<u8> = Vec::new();
    let request = loop {
        match parse_request(&buf) {
            Ok(r) => break r,
            Err(ProtoError::Incomplete) if buf.len() <= MAX_HEADER_BYTES => {
                match body.next().await {
                    Some(chunk) => buf.extend_from_slice(&chunk?),
                    None => return refuse(incomplete_error(&buf)),
                }
            }
            Err(_) => return refuse(UploadError::permanent("the request header is too long")),
        }
    };
    let bucket = env.bucket(BUCKET)?;
    match request {
        UploadRequest::GetFileSize { file_name, .. } => {
            if let Err(e) = check_get_file_size_name(&file_name) {
                return refuse(e);
            }
            if !is_flat_name(&file_name) {
                return refuse(UploadError::permanent("invalid filename"));
            }
            let size = bucket
                .head(object_key(FileKind::Upload, &file_name))
                .await?
                .map(|o| o.size())
                .unwrap_or(0);
            xml(reply_file_size(size))
        }
        UploadRequest::FileUpload(header) => {
            // A header that arrived whole in one large chunk is held to the
            // same limit as one read piece by piece.
            if header.data_offset > MAX_HEADER_BYTES {
                return refuse(UploadError::permanent("the request header is too long"));
            }
            let db = db(env)?;
            let project = match project(&db).await {
                Ok(p) => p,
                Err(e) => {
                    console_error!("settings: {e}");
                    return xml(reply_error(true, "the project is temporarily unavailable"));
                }
            };
            let ignore = project.config.ignore_upload_certificates;
            if let Err(e) = header.check_signature_fields(ignore) {
                return refuse(e);
            }
            if !ignore {
                let Some(key) = &project.upload_public_key else {
                    return refuse(UploadError::transient("the project has no upload key"));
                };
                if !verify_upload_certificate(
                    &header.name,
                    header.max_nbytes,
                    &header.xml_signature,
                    key,
                ) {
                    return refuse(UploadHeader::invalid_signature());
                }
            }
            if let Err(e) = header.check_limits(ignore) {
                return refuse(e);
            }
            if !is_flat_name(&header.name) {
                return refuse(UploadError::permanent("invalid filename"));
            }
            let prefix = buf.split_off(header.data_offset.min(buf.len()));
            store(&bucket, &header, prefix, body).await
        }
    }
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();
    match req.method() {
        Method::Get | Method::Head => match path.strip_prefix("/download/") {
            Some(name) => download(&req, &env.bucket(BUCKET)?, name).await,
            None => text(404, "not found\n"),
        },
        Method::Post if path == UPLOAD_PATH => upload(req, &env).await,
        _ => text(405, "method not allowed\n"),
    }
}
