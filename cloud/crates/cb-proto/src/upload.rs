//! The upload protocol of `sched/file_upload_handler.cpp`.
//!
//! A client posts one of two requests to the upload URL
//! (`FILE_XFER::init_upload` in `client/file_xfer.cpp`):
//!
//! - a size query, `<get_file_size>NAME</get_file_size>`, answered with
//!   `<file_size>N</file_size>`;
//! - an upload: a text header inside `<file_upload>` that ends with a
//!   `<data>` line, followed directly by the raw bytes of the file.
//!
//! [`parse_request`] reads the header from the first bytes of the body and
//! reports where the file data starts, so the data need not be in memory.
//! The request is bytes, not text: the file data after `<data>` is binary.
//!
//! The header carries the `<xml_signature>` the scheduler issued with the
//! result. The signed text is not sent; the handler rebuilds it from the
//! name and `max_nbytes` ([`UploadHeader::signed_text`]) and so binds the
//! size limit to the file name. Verifying the signature against the upload
//! public key is the caller's job.

use crate::xml::{line, xml_escape};
use crate::ProtoError;

/// A sensible bound on the size of the text header. A caller that has read
/// this many bytes and still gets [`ProtoError::Incomplete`] can reject the
/// request.
pub const MAX_HEADER_BYTES: usize = 16 * 1024;

/// The client version sent at the start of every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientVersion {
    pub major: i32,
    pub minor: i32,
    pub release: i32,
}

/// The header of a `<file_upload>` request (`handle_file_upload`).
#[derive(Debug, Clone, PartialEq)]
pub struct UploadHeader {
    pub client_version: ClientVersion,
    /// Name of the file; empty if the client sent none.
    pub name: String,
    /// Raw contents of `<xml_signature>`: hex lines ending in a `.` line.
    pub xml_signature: String,
    /// Size limit from the signed `<file_info>`; -1 if absent.
    pub max_nbytes: f64,
    /// Total size of the file; -1 if absent.
    pub nbytes: f64,
    /// Offset in the file of the first data byte; 0 unless the client
    /// resumes an upload.
    pub offset: f64,
    /// MD5 of the whole file as the client computed it; upstream ignores
    /// it.
    pub md5_cksum: String,
    /// Index in the request body of the first byte of file data.
    pub data_offset: usize,
}

/// A parsed upload request.
#[derive(Debug, Clone, PartialEq)]
pub enum UploadRequest {
    /// `<get_file_size>`: how much of this file does the server have?
    GetFileSize {
        client_version: ClientVersion,
        file_name: String,
    },
    /// `<file_upload>`: a header followed by file data.
    FileUpload(UploadHeader),
}

/// An error reply of the upload handler (`return_error`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadError {
    /// Transient errors make the client retry later; permanent ones make it
    /// give up on the file.
    pub transient: bool,
    pub message: String,
}

impl UploadError {
    pub fn permanent(message: impl Into<String>) -> Self {
        UploadError {
            transient: false,
            message: message.into(),
        }
    }

    pub fn transient(message: impl Into<String>) -> Self {
        UploadError {
            transient: true,
            message: message.into(),
        }
    }

    /// The reply body for this error.
    pub fn reply(&self) -> String {
        reply_error(self.transient, &self.message)
    }
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for UploadError {}

/// C `%.0f`.
fn fmt_f0(x: f64) -> String {
    if x.is_finite() {
        format!("{x:.0}")
    } else {
        crate::fmt_f(x)
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// One line, newline included, or `None` when the rest of `buf` holds no
/// complete line.
fn take_line(buf: &[u8], pos: usize) -> Option<&[u8]> {
    let rest = &buf[pos..];
    let n = rest.iter().position(|&c| c == b'\n')?;
    Some(&rest[..=n])
}

/// Parse the start of an upload request body (`handle_request` and
/// `handle_file_upload`).
///
/// `buf` is the first part of the body, or all of it. Returns
/// [`ProtoError::Incomplete`] when `buf` ends before the request can be
/// identified: before the `<get_file_size>` line or the `<data>` line is
/// complete. When the whole body was given, that means the request has no
/// command, which upstream answers with the transient error `no command`
/// (or the permanent error `Missing <data> tag` when a `<file_upload>` was
/// started); see [`incomplete_error`].
///
/// No field is validated here; see [`UploadHeader::check_signature_fields`]
/// and [`UploadHeader::check_limits`].
pub fn parse_request(buf: &[u8]) -> Result<UploadRequest, ProtoError> {
    let mut cv = ClientVersion::default();
    let mut pos = 0;
    loop {
        let raw = take_line(buf, pos).ok_or(ProtoError::Incomplete)?;
        pos += raw.len();
        let l = String::from_utf8_lossy(raw);
        if let Some(x) = line::parse_int(&l, "<core_client_major_version>") {
            cv.major = x;
        } else if let Some(x) = line::parse_int(&l, "<core_client_minor_version>") {
            cv.minor = x;
        } else if let Some(x) = line::parse_int(&l, "<core_client_release>") {
            cv.release = x;
        } else if line::match_tag(&l, "<file_upload>") {
            break;
        } else if let Some(file_name) = line::parse_str(&l, "<get_file_size>") {
            return Ok(UploadRequest::GetFileSize {
                client_version: cv,
                file_name,
            });
        }
    }
    let mut h = UploadHeader {
        client_version: cv,
        name: String::new(),
        xml_signature: String::new(),
        max_nbytes: -1.0,
        nbytes: -1.0,
        offset: 0.0,
        md5_cksum: String::new(),
        data_offset: 0,
    };
    loop {
        let raw = take_line(buf, pos).ok_or(ProtoError::Incomplete)?;
        pos += raw.len();
        let l = String::from_utf8_lossy(raw);
        if line::match_tag(&l, "<xml_signature>") {
            let end = b"</xml_signature>";
            let n = find(&buf[pos..], end).ok_or(ProtoError::Incomplete)?;
            h.xml_signature = String::from_utf8_lossy(&buf[pos..pos + n]).into_owned();
            pos += n + end.len();
            continue;
        }
        if let Some(s) = line::parse_str(&l, "<md5_cksum>") {
            h.md5_cksum = s;
            continue;
        }
        if let Some(s) = line::parse_str(&l, "<name>") {
            h.name = s;
            continue;
        }
        if let Some(x) = line::parse_double(&l, "<max_nbytes>") {
            h.max_nbytes = x;
            continue;
        }
        if let Some(x) = line::parse_double(&l, "<offset>") {
            h.offset = x;
            continue;
        }
        if let Some(x) = line::parse_double(&l, "<nbytes>") {
            h.nbytes = x;
            continue;
        }
        if line::match_tag(&l, "<data>") {
            h.data_offset = pos;
            return Ok(UploadRequest::FileUpload(h));
        }
    }
}

/// The error upstream returns for a complete body on which
/// [`parse_request`] reported [`ProtoError::Incomplete`].
pub fn incomplete_error(body: &[u8]) -> UploadError {
    if find(body, b"<file_upload>").is_some() {
        if find(body, b"<name>").is_none() {
            UploadError::permanent("Missing name")
        } else {
            UploadError::permanent("Missing <data> tag")
        }
    } else {
        UploadError::transient("no command")
    }
}

/// `is_valid_filename` in `lib/str_util.cpp`: no control characters, no
/// `..`, not absolute.
pub fn is_valid_filename(name: &str) -> bool {
    !name.bytes().any(|c| c.is_ascii_control()) && !name.contains("..") && !name.starts_with('/')
}

/// The check `handle_request` applies to the name of a size query.
pub fn check_get_file_size_name(file_name: &str) -> Result<(), UploadError> {
    if file_name.contains("..") {
        return Err(UploadError::permanent("Bad filename"));
    }
    Ok(())
}

impl UploadHeader {
    /// The text the scheduler signed when it issued the result:
    /// `<name>NAME</name><max_nbytes>N</max_nbytes>`. `xml_signature` must
    /// be a valid signature of this text.
    pub fn signed_text(&self) -> String {
        format!(
            "<name>{}</name><max_nbytes>{}</max_nbytes>",
            self.name,
            fmt_f0(self.max_nbytes)
        )
    }

    /// The checks upstream makes before verifying the signature: a name,
    /// a signature and a size limit are present. Skip the signature parts
    /// with `ignore_upload_certificates`.
    pub fn check_signature_fields(
        &self,
        ignore_upload_certificates: bool,
    ) -> Result<(), UploadError> {
        if self.name.is_empty() {
            return Err(UploadError::permanent("Missing name"));
        }
        if !ignore_upload_certificates {
            if self.xml_signature.is_empty() {
                return Err(UploadError::permanent("missing signature"));
            }
            if self.max_nbytes < 0.0 {
                return Err(UploadError::permanent("missing max_nbytes"));
            }
        }
        Ok(())
    }

    /// The error for a signature that does not verify.
    pub fn invalid_signature() -> UploadError {
        UploadError::permanent("invalid signature")
    }

    /// The checks upstream makes after verifying the signature: `nbytes`
    /// is present, within the signed limit, and the name is safe to use
    /// in a path. `max_upload_nbytes` is the project's own bound, which
    /// holds with or without a certificate.
    pub fn check_limits(
        &self,
        ignore_upload_certificates: bool,
        max_upload_nbytes: f64,
    ) -> Result<(), UploadError> {
        // NaN is rejected too; upstream's `nbytes < 0` would let it pass.
        if self.nbytes.is_nan() || self.nbytes < 0.0 {
            return Err(UploadError::permanent("nbytes missing or negative"));
        }
        if self.nbytes > max_upload_nbytes {
            return Err(UploadError::permanent(format!(
                "file size ({} KB) exceeds the project limit ({} KB)",
                (self.nbytes / 1024.0) as i64,
                (max_upload_nbytes / 1024.0) as i64
            )));
        }
        if !ignore_upload_certificates && self.nbytes > self.max_nbytes {
            return Err(UploadError::permanent(format!(
                "file size ({} KB) exceeds limit ({} KB)",
                (self.nbytes / 1024.0) as i32,
                (self.max_nbytes / 1024.0) as i32
            )));
        }
        if !is_valid_filename(&self.name) {
            return Err(UploadError::permanent(format!(
                "file_upload_handler: invalid filename: {}",
                self.name
            )));
        }
        Ok(())
    }
}

/// Reply to a successful upload (`return_success(0)`).
pub fn reply_success() -> String {
    "<data_server_reply>\n    <status>0</status>\n</data_server_reply>\n".to_string()
}

/// Reply to a size query (`handle_get_file_size`). A file the server does
/// not have has size 0.
pub fn reply_file_size(nbytes: u64) -> String {
    format!(
        "<data_server_reply>\n    <status>0</status>\n    <file_size>{nbytes}</file_size>\n</data_server_reply>\n"
    )
}

/// Error reply (`return_error`): status 1 for a transient error, -1 for a
/// permanent one.
///
/// The message is escaped. Upstream writes it as is, and the client then
/// shows only the text before the first `<`, so `Missing <data> tag` reaches
/// the user as `Missing`; the client unescapes the message it reads
/// (`parse_str` in `lib/parse.cpp`), so the escaped form shows in full.
pub fn reply_error(transient: bool, message: &str) -> String {
    format!(
        "<data_server_reply>\n    <status>{}</status>\n    <message>{}</message>\n</data_server_reply>\n",
        if transient { 1 } else { -1 },
        xml_escape(message)
    )
}

/// A reply as the client reads it (`FILE_XFER::parse_upload_response` in
/// `client/file_xfer.cpp`).
#[derive(Debug, Clone, PartialEq)]
pub struct UploadReply {
    /// 0 for success, -1 for a permanent error, anything else transient.
    /// `None` when the reply has no `<status>`, as for an HTML error
    /// page.
    pub status: Option<i32>,
    /// `<file_size>`; -1 if absent.
    pub file_size: f64,
    pub message: String,
}

/// Parse a reply the way the client does.
pub fn parse_reply(text: &str) -> UploadReply {
    let mut r = UploadReply {
        status: None,
        file_size: -1.0,
        message: String::new(),
    };
    if let Some(x) = line::parse_double(text, "<file_size>") {
        r.file_size = x;
    }
    r.status = line::parse_int(text, "<status>");
    if let Some(s) = line::parse_str(text, "<message>") {
        r.message = s;
    }
    r
}

/// The size query a client sends (`FILE_XFER::init_upload`).
pub fn write_get_file_size_request(cv: ClientVersion, file_name: &str) -> String {
    format!(
        "<data_server_request>\n    <core_client_major_version>{}</core_client_major_version>\n    <core_client_minor_version>{}</core_client_minor_version>\n    <core_client_release>{}</core_client_release>\n    <get_file_size>{}</get_file_size>\n</data_server_request>\n",
        cv.major, cv.minor, cv.release, file_name
    )
}

/// The header a client sends before the file data
/// (`FILE_XFER::init_upload`). `xml_signature` ends in a newline.
pub fn write_upload_header(
    cv: ClientVersion,
    name: &str,
    xml_signature: &str,
    max_nbytes: f64,
    nbytes: f64,
    md5_cksum: &str,
    offset: f64,
) -> String {
    format!(
        "<data_server_request>\n    <core_client_major_version>{}</core_client_major_version>\n    <core_client_minor_version>{}</core_client_minor_version>\n    <core_client_release>{}</core_client_release>\n<file_upload>\n<file_info>\n<name>{}</name>\n<xml_signature>\n{}</xml_signature>\n<max_nbytes>{}</max_nbytes>\n</file_info>\n<nbytes>{}</nbytes>\n<md5_cksum>{}</md5_cksum>\n<offset>{}</offset>\n<data>\n",
        cv.major,
        cv.minor,
        cv.release,
        name,
        xml_signature,
        fmt_f0(max_nbytes),
        fmt_f0(nbytes),
        md5_cksum,
        fmt_f0(offset)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CV: ClientVersion = ClientVersion {
        major: 8,
        minor: 2,
        release: 4,
    };
    const SIG: &str = "0f3c5a\n9b77e1\n.\n";

    fn upload_body(data: &[u8]) -> Vec<u8> {
        let mut b = write_upload_header(
            CV,
            "wu_1_0_r77_0",
            SIG,
            1_000_000.0,
            data.len() as f64,
            "d41d8cd98f00b204e9800998ecf8427e",
            0.0,
        )
        .into_bytes();
        b.extend_from_slice(data);
        b
    }

    #[test]
    fn client_header_bytes() {
        assert_eq!(
            write_upload_header(CV, "f", "ab\n.\n", 100.0, 5.0, "cc", 0.0),
            "<data_server_request>\n    <core_client_major_version>8</core_client_major_version>\n    <core_client_minor_version>2</core_client_minor_version>\n    <core_client_release>4</core_client_release>\n<file_upload>\n<file_info>\n<name>f</name>\n<xml_signature>\nab\n.\n</xml_signature>\n<max_nbytes>100</max_nbytes>\n</file_info>\n<nbytes>5</nbytes>\n<md5_cksum>cc</md5_cksum>\n<offset>0</offset>\n<data>\n"
        );
    }

    #[test]
    fn size_query() {
        let body = write_get_file_size_request(CV, "wu_1_0_r77_0");
        assert_eq!(
            parse_request(body.as_bytes()).unwrap(),
            UploadRequest::GetFileSize {
                client_version: CV,
                file_name: "wu_1_0_r77_0".to_string(),
            }
        );
        assert!(check_get_file_size_name("wu_1_0_r77_0").is_ok());
        let e = check_get_file_size_name("../../etc/passwd").unwrap_err();
        assert!(!e.transient);
        assert_eq!(e.message, "Bad filename");
    }

    #[test]
    fn upload_header_and_data_offset() {
        // Binary data that looks like header text must not be parsed.
        let data = b"\x00\xff\x01<nbytes>999</nbytes>\n<data>\nrest";
        let body = upload_body(data);
        let UploadRequest::FileUpload(h) = parse_request(&body).unwrap() else {
            panic!("not an upload");
        };
        assert_eq!(h.client_version, CV);
        assert_eq!(h.name, "wu_1_0_r77_0");
        assert_eq!(h.xml_signature, SIG);
        assert_eq!(h.max_nbytes, 1e6);
        assert_eq!(h.nbytes, data.len() as f64);
        assert_eq!(h.offset, 0.0);
        assert_eq!(h.md5_cksum, "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(&body[h.data_offset..], data);
        assert_eq!(
            h.signed_text(),
            "<name>wu_1_0_r77_0</name><max_nbytes>1000000</max_nbytes>"
        );
        assert!(h.check_signature_fields(false).is_ok());
        assert!(h.check_limits(false, 1e9).is_ok());
    }

    #[test]
    fn every_prefix_of_the_header_is_incomplete() {
        let body = upload_body(b"payload");
        let header_len = body.len() - b"payload".len();
        for n in 0..header_len {
            assert_eq!(
                parse_request(&body[..n]),
                Err(ProtoError::Incomplete),
                "prefix of {n} bytes"
            );
        }
        // The header alone is enough.
        let UploadRequest::FileUpload(h) = parse_request(&body[..header_len]).unwrap() else {
            panic!("not an upload");
        };
        assert_eq!(h.data_offset, header_len);

        let q = write_get_file_size_request(CV, "abc");
        let done = q.find("</get_file_size>\n").unwrap() + "</get_file_size>\n".len();
        for n in 0..done {
            assert_eq!(
                parse_request(&q.as_bytes()[..n]),
                Err(ProtoError::Incomplete)
            );
        }
        assert!(parse_request(&q.as_bytes()[..done]).is_ok());
    }

    #[test]
    fn bodies_without_a_command() {
        for body in [
            "",
            "hello\n",
            "<data_server_request>\n</data_server_request>\n",
        ] {
            assert_eq!(parse_request(body.as_bytes()), Err(ProtoError::Incomplete));
            let e = incomplete_error(body.as_bytes());
            assert!(e.transient);
            assert_eq!(e.message, "no command");
        }
        let body = "<data_server_request>\n<file_upload>\n<name>x</name>\n<nbytes>1</nbytes>\n";
        assert_eq!(parse_request(body.as_bytes()), Err(ProtoError::Incomplete));
        let e = incomplete_error(body.as_bytes());
        assert!(!e.transient);
        assert_eq!(e.message, "Missing <data> tag");
        let e = incomplete_error(b"<file_upload>\n<nbytes>1</nbytes>\n");
        assert_eq!(e.message, "Missing name");
        // A signature that is never closed.
        let body = "<file_upload>\n<name>x</name>\n<xml_signature>\nabc\n<data>\n";
        assert_eq!(parse_request(body.as_bytes()), Err(ProtoError::Incomplete));
    }

    #[test]
    fn old_client_header_with_extra_fields() {
        // Old clients sent the whole <file_info>, and CRLF does no harm.
        let body = b"<data_server_request>\r\n<core_client_major_version>5</core_client_major_version>\r\n<file_upload>\n<file_info>\n<name>old_0</name>\n<generated_locally/>\n<upload_when_present/>\n<url>http://example.org/cgi/fuh</url>\n<max_nbytes>4096</max_nbytes>\n<signed_xml>\n</signed_xml>\n<xml_signature>\naa\n.\n</xml_signature>\n</file_info>\n<nbytes>10</nbytes>\n<offset>4</offset>\nsomething odd\n<data>\nabcdef";
        let UploadRequest::FileUpload(h) = parse_request(body).unwrap() else {
            panic!("not an upload");
        };
        assert_eq!(h.client_version.major, 5);
        assert_eq!(h.name, "old_0");
        assert_eq!(h.max_nbytes, 4096.0);
        assert_eq!(h.nbytes, 10.0);
        assert_eq!(h.offset, 4.0);
        assert_eq!(h.xml_signature, "aa\n.\n");
        assert_eq!(h.md5_cksum, "");
        assert_eq!(&body[h.data_offset..], b"abcdef");
    }

    fn header(text: &str) -> UploadHeader {
        match parse_request(text.as_bytes()).unwrap() {
            UploadRequest::FileUpload(h) => h,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn validation_errors() {
        let h = header("<file_upload>\n<nbytes>1</nbytes>\n<data>\n");
        assert_eq!(h.max_nbytes, -1.0);
        assert_eq!(
            h.check_signature_fields(false),
            Err(UploadError::permanent("Missing name"))
        );

        let h = header("<file_upload>\n<name>a</name>\n<data>\n");
        assert_eq!(
            h.check_signature_fields(false),
            Err(UploadError::permanent("missing signature"))
        );
        assert!(h.check_signature_fields(true).is_ok());
        assert_eq!(
            h.check_limits(true, 1e9),
            Err(UploadError::permanent("nbytes missing or negative"))
        );

        let h = header(
            "<file_upload>\n<name>a</name>\n<xml_signature>\nab\n.\n</xml_signature>\n<data>\n",
        );
        assert_eq!(
            h.check_signature_fields(false),
            Err(UploadError::permanent("missing max_nbytes"))
        );

        let h = header(
            "<file_upload>\n<name>a</name>\n<max_nbytes>2048</max_nbytes>\n<nbytes>4096</nbytes>\n<data>\n",
        );
        assert_eq!(
            h.check_limits(false, 1e9),
            Err(UploadError::permanent(
                "file size (4 KB) exceeds limit (2 KB)"
            ))
        );
        assert!(h.check_limits(true, 1e9).is_ok());
        // The project's bound holds when certificates are ignored.
        assert!(h.check_limits(true, 4096.0).is_ok());
        assert_eq!(
            h.check_limits(true, 4095.0),
            Err(UploadError::permanent(
                "file size (4 KB) exceeds the project limit (3 KB)"
            ))
        );

        for bad in ["../a", "a/../b", "/abs", "a\tb"] {
            let h = header(&format!(
                "<file_upload>\n<name>{bad}</name>\n<max_nbytes>10</max_nbytes>\n<nbytes>1</nbytes>\n<data>\n"
            ));
            let e = h.check_limits(false, 1e9).unwrap_err();
            assert!(!e.transient);
            assert!(
                e.message
                    .starts_with("file_upload_handler: invalid filename"),
                "{bad}"
            );
        }
        let h = header("<file_upload>\n<name>a</name>\n<max_nbytes>10</max_nbytes>\n<nbytes>nan</nbytes>\n<data>\n");
        assert!(h.check_limits(false, 1e9).is_err());
        assert_eq!(
            UploadHeader::invalid_signature().message,
            "invalid signature"
        );
    }

    #[test]
    fn signed_text_uses_whole_numbers() {
        let h = header("<file_upload>\n<name>n</name>\n<max_nbytes>1e9</max_nbytes>\n<data>\n");
        assert_eq!(
            h.signed_text(),
            "<name>n</name><max_nbytes>1000000000</max_nbytes>"
        );
        let h = header("<file_upload>\n<name>n</name>\n<max_nbytes>2.5</max_nbytes>\n<data>\n");
        assert_eq!(h.signed_text(), "<name>n</name><max_nbytes>2</max_nbytes>");
    }

    #[test]
    fn replies() {
        assert_eq!(
            reply_success(),
            "<data_server_reply>\n    <status>0</status>\n</data_server_reply>\n"
        );
        assert_eq!(
            reply_file_size(1234),
            "<data_server_reply>\n    <status>0</status>\n    <file_size>1234</file_size>\n</data_server_reply>\n"
        );
        assert_eq!(
            reply_error(true, "no command"),
            "<data_server_reply>\n    <status>1</status>\n    <message>no command</message>\n</data_server_reply>\n"
        );
        assert_eq!(
            UploadError::permanent("Bad filename").reply(),
            "<data_server_reply>\n    <status>-1</status>\n    <message>Bad filename</message>\n</data_server_reply>\n"
        );
    }

    #[test]
    fn replies_as_the_client_reads_them() {
        let r = parse_reply(&reply_success());
        assert_eq!(r.status, Some(0));
        assert_eq!(r.file_size, -1.0);
        let r = parse_reply(&reply_file_size(0));
        assert_eq!((r.status, r.file_size), (Some(0), 0.0));
        let r = parse_reply(&reply_file_size(5_000_000_000));
        assert_eq!(r.file_size, 5e9);
        let r = parse_reply(&reply_error(false, "invalid signature"));
        assert_eq!(r.status, Some(-1));
        assert_eq!(r.message, "invalid signature");
        let r = parse_reply(&reply_error(true, "busy"));
        assert_eq!(r.status, Some(1));
        let r = parse_reply("<html>502</html>");
        assert_eq!(r.status, None);
    }

    #[test]
    fn error_messages_with_markup_reach_the_client_whole() {
        let text = reply_error(false, "Missing <data> tag & more");
        assert_eq!(
            text,
            "<data_server_reply>\n    <status>-1</status>\n    <message>Missing &lt;data> tag &amp; more</message>\n</data_server_reply>\n"
        );
        // `parse_str` stops at the first `<`, as the client's does.
        assert_eq!(parse_reply(&text).message, "Missing <data> tag & more");
        assert_eq!(
            incomplete_error(b"<file_upload>\n<name>x</name>\n").reply(),
            "<data_server_reply>\n    <status>-1</status>\n    <message>Missing &lt;data> tag</message>\n</data_server_reply>\n"
        );
    }

    #[test]
    fn filenames() {
        assert!(is_valid_filename("wu_1_0_r77_0"));
        assert!(is_valid_filename("a/b"));
        assert!(!is_valid_filename("a..b"));
        assert!(!is_valid_filename("/a"));
        assert!(!is_valid_filename("a\nb"));
        assert!(!is_valid_filename("a\x7fb"));
    }
}
