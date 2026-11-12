//! The URL layout of a stock BOINC project server, as an option.
//!
//! A server made with `make_project` under the short name `<short>` serves
//! the scheduler at `/<short>_cgi/cgi`, the upload handler at
//! `/<short>_cgi/file_upload_handler`, downloads at
//! `/<short>/download/<dir>/<name>` and the user pages at `/<short>/`.
//! Every attached client holds those URLs, in the project state and in the
//! `<file_info>` elements of the work it already has. A deployment that takes
//! over such a project's master URL answers on the same paths, so no client
//! has to detach and attach again.
//!
//! This module holds the settings, the path mapping and the routing decisions
//! as plain functions. The Workers only call them. Off by default: with no
//! `stock_*` setting in the config, every function here sees `None` and the
//! routes are the ones of the native layout.
//!
//! Upstream references: `dir_hier_path` and `filename_hash` in
//! `sched/sched_util_basic.cpp` (the directory of a file),
//! `tools/make_project` and `py/Boinc/setup_project.py` (the default URLs),
//! `SCHED_CONFIG::parse` in `sched/sched_config.cpp` (`uldl_dir_fanout`).

use cb_crypto::md5::md5_hex;

use crate::{CoreError, CoreResult};

/// Last path segment of the upload handler URL. A client takes a `<url>` for
/// an upload URL only when it contains this text.
pub const UPLOAD_HANDLER: &str = "file_upload_handler";
/// Last path segment of the scheduler URL.
pub const SCHEDULER_CGI: &str = "cgi";

/// Path segments the native layout uses at the root. A stock page prefix
/// may not shadow them.
const RESERVED_FIRST_SEGMENTS: [&str; 4] = ["admin", "download", "sched", "upload"];

/// Where a stock project's URLs lie, as paths on the master URL's host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StockLayout {
    /// `/<short>_cgi`: holds `cgi`, the scheduler.
    pub cgi_prefix: String,
    /// The directory of `file_upload_handler`: `/<short>_cgi` for a project
    /// made with `make_project`, whose `upload_url` is `<cgi_url>/file_upload_handler`.
    /// A project that set `upload_url` elsewhere names that directory.
    pub upload_prefix: String,
    /// `/<short>/download`: holds `<dir>/<name>`, or `<name>` without fanout.
    pub download_prefix: String,
    /// `/<short>`: the user pages. Empty when the pages are only at the root.
    pub page_prefix: String,
    /// `uldl_dir_fanout`: 0 for a flat download directory.
    pub fanout: u32,
    /// Whether new replies carry the stock URLs (the scheduler link on the
    /// master page, the URLs of files in new work) or the native ones. The
    /// stock paths are served either way.
    pub advertise: bool,
}

/// The `stock_*` tags of the `<config>` element, before defaults are
/// applied.
#[derive(Debug, Clone, Default)]
pub struct StockSpec {
    pub short_name: String,
    pub cgi_prefix: String,
    /// Directory of the upload handler; the cgi prefix when not given.
    pub upload_prefix: String,
    pub download_prefix: String,
    /// `None`: not given. `Some("")` or `Some("/")`: no page prefix.
    pub page_prefix: Option<String>,
    pub advertise: Option<bool>,
}

/// A path prefix as the config writes it: `/` and then segments, no query,
/// no trailing slash, no `.` or `..`.
fn clean_prefix(tag: &str, value: &str) -> CoreResult<String> {
    let v = value.trim().trim_end_matches('/');
    let ok = v.starts_with('/')
        && v[1..].split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~'))
        });
    if !ok {
        return Err(CoreError::Invalid(format!(
            "config: <{tag}> must be a path such as /name or /name/sub, got {value:?}"
        )));
    }
    Ok(v.to_string())
}

fn short_name_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

impl StockSpec {
    /// Whether any stock tag is present.
    pub fn is_set(&self) -> bool {
        !self.short_name.is_empty()
            || !self.cgi_prefix.is_empty()
            || !self.upload_prefix.is_empty()
            || !self.download_prefix.is_empty()
            || self.page_prefix.is_some()
            || self.advertise.is_some()
    }

    /// The layout, with the `make_project` defaults for what the spec leaves
    /// out. `None` when no stock tag is set.
    pub fn resolve(&self, fanout: i32) -> CoreResult<Option<StockLayout>> {
        if !self.is_set() {
            return Ok(None);
        }
        let short = self.short_name.trim();
        if !short.is_empty() && !short_name_ok(short) {
            return Err(CoreError::Invalid(format!(
                "config: <stock_short_name> may hold letters, digits, - and _ only, got {short:?}"
            )));
        }
        let derive = |tag: &str, explicit: &str, suffix: &str| -> CoreResult<String> {
            if !explicit.trim().is_empty() {
                clean_prefix(tag, explicit)
            } else if !short.is_empty() {
                Ok(format!("/{short}{suffix}"))
            } else {
                Err(CoreError::Invalid(format!(
                    "config: set <stock_short_name>, or <{tag}> and the other stock prefixes"
                )))
            }
        };
        let cgi_prefix = derive("stock_cgi_prefix", &self.cgi_prefix, "_cgi")?;
        let upload_prefix = if self.upload_prefix.trim().is_empty() {
            cgi_prefix.clone()
        } else {
            clean_prefix("stock_upload_prefix", &self.upload_prefix)?
        };
        let download_prefix = derive("stock_download_prefix", &self.download_prefix, "/download")?;
        let page_prefix = match &self.page_prefix {
            Some(p) if p.trim().is_empty() || p.trim() == "/" => String::new(),
            Some(p) => clean_prefix("stock_page_prefix", p)?,
            None if !short.is_empty() => format!("/{short}"),
            None => String::new(),
        };
        if fanout < 0 {
            return Err(CoreError::Invalid(
                "config: <uldl_dir_fanout> must not be negative".into(),
            ));
        }
        // The paths the layout serves: the three prefixes, the page prefix and
        // the two endpoints, each of which gets a route (see
        // `StockLayout::routes`). Two equal paths would need one route
        // pattern for two destinations, and a page prefix that is, or is
        // under, an endpoint path would be answered by the endpoint's route
        // or hide it. The cgi and upload directories may be one, which is the
        // default. Other nesting is fine: the longer route is the more
        // specific.
        let scheduler_path = format!("{cgi_prefix}/{SCHEDULER_CGI}");
        let handler_path = format!("{upload_prefix}/{UPLOAD_HANDLER}");
        let named: [(&str, &String); 6] = [
            ("stock_page_prefix", &page_prefix),
            ("stock_cgi_prefix", &cgi_prefix),
            ("stock_upload_prefix", &upload_prefix),
            ("stock_download_prefix", &download_prefix),
            ("the scheduler path", &scheduler_path),
            ("the upload handler path", &handler_path),
        ];
        for (i, (name_a, a)) in named.iter().enumerate() {
            for (name_b, b) in &named[i + 1..] {
                let cgi_and_upload =
                    *name_a == "stock_cgi_prefix" && *name_b == "stock_upload_prefix";
                if a == b && !a.is_empty() && !cgi_and_upload {
                    return Err(CoreError::Invalid(format!(
                        "config: {name_a} and {name_b} are both {a}; they must differ"
                    )));
                }
            }
        }
        for (what, endpoint) in [
            ("the scheduler path", &scheduler_path),
            ("the upload handler path", &handler_path),
        ] {
            if !page_prefix.is_empty() && page_prefix.starts_with(&format!("{endpoint}/")) {
                return Err(CoreError::Invalid(format!(
                    "config: <stock_page_prefix> {page_prefix} is under {what} {endpoint}"
                )));
            }
        }
        for (tag, p) in [
            ("stock_cgi_prefix", &cgi_prefix),
            ("stock_upload_prefix", &upload_prefix),
            ("stock_page_prefix", &page_prefix),
        ] {
            let first = p.trim_start_matches('/').split('/').next().unwrap_or("");
            if RESERVED_FIRST_SEGMENTS.contains(&first) {
                return Err(CoreError::Invalid(format!(
                    "config: <{tag}> may not start with /{first}, which the native layout uses"
                )));
            }
        }
        // `/admin/` belongs to the admin Worker, so nothing of the layout may be
        // under it.
        let first = download_prefix
            .trim_start_matches('/')
            .split('/')
            .next()
            .unwrap_or("");
        if first == "admin" {
            return Err(CoreError::Invalid(
                "config: <stock_download_prefix> may not start with /admin, which the admin \
                 API uses"
                    .into(),
            ));
        }
        Ok(Some(StockLayout {
            cgi_prefix,
            upload_prefix,
            download_prefix,
            page_prefix,
            fanout: fanout as u32,
            advertise: self.advertise.unwrap_or(true),
        }))
    }
}

/// The directory name `filename_hash` gives a file when the download
/// hierarchy has `fanout` directories: the lowercase hex of the number that
/// hex digits 1 to 7 of the MD5 of the file name make, modulo `fanout`.
/// `None` for `fanout` 0, where the hierarchy is flat.
pub fn filename_dir(name: &str, fanout: u32) -> Option<String> {
    if fanout == 0 {
        return None;
    }
    let md5 = md5_hex(name.as_bytes());
    // Seven hex digits fit in 28 bits, so upstream's `int` does not overflow.
    let x = u32::from_str_radix(&md5[1..8], 16).unwrap_or(0);
    Some(format!("{:x}", x % fanout))
}

/// What a download URL's path holds below the prefix: `<name>` or
/// `<dir>/<name>`. Builds the part after `<prefix>/` for a file.
pub fn download_tail(name: &str, fanout: u32) -> String {
    match filename_dir(name, fanout) {
        Some(dir) => format!("{dir}/{name}"),
        None => name.to_string(),
    }
}

/// The URLs of the files a project serves: `<base>/<tail>`, where the tail
/// has a hash directory when the base is a stock download URL with fanout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUrls {
    pub base: String,
    pub fanout: u32,
}

impl FileUrls {
    /// The URL of a download file, as `dir_hier_url` writes it.
    pub fn download(&self, name: &str) -> String {
        format!("{}/{}", self.base, download_tail(name, self.fanout))
    }
}

/// `scheme://host[:port]` of a URL, without a trailing slash.
pub fn url_origin(url: &str) -> Option<&str> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find('/').unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(&url[..scheme.len() + 3 + end])
}

/// The path of a URL, without a trailing slash; `""` for none.
pub fn url_path(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    match rest.find('/') {
        Some(i) => rest[i..].trim_end_matches('/'),
        None => "",
    }
}

/// A URL without its scheme and with a lowercase host, for comparing the
/// download URLs of stored XML with the ones this project serves: a project
/// that moved from `http` to `https` keeps its work.
pub fn comparable_url(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    match rest.find('/') {
        Some(i) => format!("{}{}", rest[..i].to_ascii_lowercase(), &rest[i..]),
        None => rest.to_ascii_lowercase(),
    }
}

impl StockLayout {
    /// `<cgi_prefix>/cgi`
    pub fn scheduler_path(&self) -> String {
        format!("{}/{SCHEDULER_CGI}", self.cgi_prefix)
    }

    /// `<upload_prefix>/file_upload_handler`
    pub fn upload_path(&self) -> String {
        format!("{}/{UPLOAD_HANDLER}", self.upload_prefix)
    }

    pub fn is_scheduler_path(&self, path: &str) -> bool {
        path.strip_prefix(&self.cgi_prefix)
            .and_then(|r| r.strip_prefix('/'))
            == Some(SCHEDULER_CGI)
    }

    pub fn is_upload_path(&self, path: &str) -> bool {
        path.strip_prefix(&self.upload_prefix)
            .and_then(|r| r.strip_prefix('/'))
            == Some(UPLOAD_HANDLER)
    }

    /// The file a download path names: `<download_prefix>/<dir>/<name>`, or
    /// `<download_prefix>/<name>` without fanout. `<dir>` must be the one
    /// `filename_dir` gives. A wrong directory is `None`, as a stock server
    /// answers 404 for a file that is not where the hierarchy puts it. The
    /// check also keeps one object from having many cacheable URLs.
    pub fn download_name<'a>(&self, path: &'a str) -> Option<&'a str> {
        let tail = path
            .strip_prefix(&self.download_prefix)?
            .strip_prefix('/')?;
        if self.fanout == 0 {
            return (!tail.is_empty() && !tail.contains('/')).then_some(tail);
        }
        let (dir, name) = tail.split_once('/')?;
        if name.is_empty() || name.contains('/') {
            return None;
        }
        (filename_dir(name, self.fanout).as_deref() == Some(dir)).then_some(name)
    }

    /// The URL clients use for a download file, on the master URL's host.
    pub fn download_url(&self, origin: &str) -> String {
        format!("{origin}{}", self.download_prefix)
    }

    /// The URL of the upload handler, on the master URL's host.
    pub fn upload_url(&self, origin: &str) -> String {
        format!("{origin}{}", self.upload_path())
    }

    /// The URL of the scheduler, on the master URL's host.
    pub fn scheduler_url(&self, origin: &str) -> String {
        format!("{origin}{}", self.scheduler_path())
    }

    /// The directory, with a trailing slash, that a page served under the
    /// page prefix is in: what its relative links, its redirects and its
    /// cookie path refer to.
    pub fn page_base(&self, origin: &str) -> String {
        format!("{origin}{}/", self.page_prefix)
    }
}

/// How the files Worker treats a request path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilesRoute<'a> {
    /// `GET`/`HEAD` of the download file with this name.
    Download(&'a str),
    /// The upload handler: size query, upload and resume.
    Upload,
    NotFound,
}

/// Routes a path for the files Worker. The native paths need no setting; the
/// stock ones only match when `stock` is given, so a caller tries
/// `route_files(path, None)` first and loads the settings only on
/// `NotFound`. A path below `/download/` with a further `/` is not a native
/// name, so with a stock download prefix of `/download` it is routed as a
/// stock download.
pub fn route_files<'a>(path: &'a str, stock: Option<&StockLayout>) -> FilesRoute<'a> {
    if path == "/upload/file_upload_handler" {
        return FilesRoute::Upload;
    }
    // A native name is flat. A longer path may be a stock download below a
    // stock download prefix that starts with `/download`, so it is left to
    // the stock routing below.
    if let Some(name) = path.strip_prefix("/download/").filter(|n| !n.contains('/')) {
        return FilesRoute::Download(name);
    }
    if let Some(s) = stock {
        if s.is_upload_path(path) {
            return FilesRoute::Upload;
        }
        if let Some(name) = s.download_name(path) {
            return FilesRoute::Download(name);
        }
    }
    FilesRoute::NotFound
}

/// Whether the scheduler Worker answers a request path: `/sched`, or the
/// stock scheduler path. The route `/sched*` also delivers other paths that
/// begin with those letters (a stock short name such as `scheduler`), which
/// are not the scheduler's.
pub fn route_scheduler(path: &str, stock: Option<&StockLayout>) -> bool {
    path == "/sched" || stock.is_some_and(|s| s.is_scheduler_path(path))
}

/// The Worker a route sends to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteTarget {
    Scheduler,
    Files,
    Web,
    Admin,
}

/// The native routes of a deployment (`wrangler.toml` of each Worker), as
/// path patterns on the hostname. A trailing `*` matches any rest.
pub const NATIVE_ROUTES: [(&str, RouteTarget); 5] = [
    ("/sched*", RouteTarget::Scheduler),
    ("/upload*", RouteTarget::Files),
    ("/download/*", RouteTarget::Files),
    ("/admin/*", RouteTarget::Admin),
    ("/*", RouteTarget::Web),
];

impl StockLayout {
    /// The routes a deployment adds for this layout, as path patterns, with
    /// the Worker each goes to; `docs/deploy.md` lists them. Each is meant to
    /// be more specific than the native route it overlaps: a short name that
    /// begins with `sched` or `upload` has its pages under a path that the
    /// native `/sched*` or `/upload*` also matches, so the pages need routes
    /// of their own. The page prefix without its slash, which is redirected to
    /// the one with it, has a wildcard too: Cloudflare matches the query
    /// string as part of the URL, so an exact pattern would miss
    /// `/<prefix>?x=1`. Cloudflare sends a request to the route with the most
    /// specific pattern, and a longer literal part is more specific. The
    /// validation in `StockSpec::resolve` refuses the layouts in which two
    /// routes would tie, or a page route would capture an endpoint.
    pub fn routes(&self) -> Vec<(String, RouteTarget)> {
        let mut routes = vec![
            (format!("{}/cgi*", self.cgi_prefix), RouteTarget::Scheduler),
            (
                format!("{}/{UPLOAD_HANDLER}*", self.upload_prefix),
                RouteTarget::Files,
            ),
            (format!("{}/*", self.download_prefix), RouteTarget::Files),
        ];
        if !self.page_prefix.is_empty() {
            routes.push((format!("{}*", self.page_prefix), RouteTarget::Web));
            routes.push((format!("{}/*", self.page_prefix), RouteTarget::Web));
        }
        routes
    }
}

/// How the web Worker treats a request path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebRoute<'a> {
    /// A page or RPC. `path` is the path at the root, which is how the
    /// handlers name pages; `prefixed` says the request came in under the
    /// stock page prefix.
    Page { path: &'a str, prefixed: bool },
    /// The page prefix without its trailing slash: the path to send the
    /// client to, as a web server does for a directory.
    Redirect(String),
}

/// Routes a path for the web Worker. Pages answer at the root and, with a
/// stock page prefix, under it.
pub fn route_web<'a>(path: &'a str, stock: Option<&StockLayout>) -> WebRoute<'a> {
    if let Some(s) = stock.filter(|s| !s.page_prefix.is_empty()) {
        if path == s.page_prefix {
            return WebRoute::Redirect(format!("{path}/"));
        }
        if let Some(rest) = path
            .strip_prefix(&s.page_prefix)
            .filter(|r| r.starts_with('/'))
        {
            return WebRoute::Page {
                path: rest,
                prefixed: true,
            };
        }
    }
    WebRoute::Page {
        path,
        prefixed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(short: &str) -> StockSpec {
        StockSpec {
            short_name: short.into(),
            ..Default::default()
        }
    }

    fn layout(fanout: i32) -> StockLayout {
        spec("testproj").resolve(fanout).unwrap().unwrap()
    }

    /// Values from upstream `filename_hash` in `sched/sched_util_basic.cpp`,
    /// compiled from that file with `lib/md5_file.cpp` and run on each name
    /// and fanout; they also equal `get_output_file_path` in
    /// `py/Boinc/tools.py`.
    const UPSTREAM_DIRS: &[(&str, u32, &str)] = &[
        ("wu_1_0", 1024, "319"),
        ("testproj_job_000042_0", 1024, "218"),
        ("testproj_job_000042_0_0", 1024, "259"),
        ("a", 1024, "1b9"),
        ("jf_d41d8cd98f00b204e9800998ecf8427e", 1024, "28f"),
        ("app_1.00_x86_64-pc-linux-gnu", 1024, "1a5"),
        ("libfoo.so.1", 1024, "163"),
        ("0", 1024, "84"),
        ("zzzzzzzz_9999999_r1234567_1", 1024, "11e"),
        ("UPPER_Case.Name-1", 1024, "33d"),
        ("wu_1_0", 16, "9"),
        ("testproj_job_000042_0", 16, "8"),
        ("testproj_job_000042_0_0", 16, "9"),
        ("a", 16, "9"),
        ("jf_d41d8cd98f00b204e9800998ecf8427e", 16, "f"),
        ("app_1.00_x86_64-pc-linux-gnu", 16, "5"),
        ("libfoo.so.1", 16, "3"),
        ("0", 16, "4"),
        ("zzzzzzzz_9999999_r1234567_1", 16, "e"),
        ("UPPER_Case.Name-1", 16, "d"),
        ("wu_1_0", 100, "1d"),
        ("testproj_job_000042_0", 100, "34"),
        ("testproj_job_000042_0_0", 100, "11"),
        ("a", 100, "4d"),
        ("jf_d41d8cd98f00b204e9800998ecf8427e", 100, "27"),
        ("app_1.00_x86_64-pc-linux-gnu", 100, "41"),
        ("libfoo.so.1", 100, "f"),
        ("0", 100, "2c"),
        ("zzzzzzzz_9999999_r1234567_1", 100, "2"),
        ("UPPER_Case.Name-1", 100, "5d"),
        ("wu_1_0", 1, "0"),
        ("a", 1, "0"),
        ("wu_1_0", 7, "5"),
        ("testproj_job_000042_0", 7, "4"),
        ("testproj_job_000042_0_0", 7, "0"),
        ("a", 7, "1"),
        ("jf_d41d8cd98f00b204e9800998ecf8427e", 7, "6"),
        ("0", 7, "6"),
    ];

    #[test]
    fn directories_match_upstream_filename_hash() {
        for (name, fanout, dir) in UPSTREAM_DIRS {
            assert_eq!(
                filename_dir(name, *fanout).as_deref(),
                Some(*dir),
                "{name} at fanout {fanout}"
            );
        }
        // Upstream gives 221 for a name of 200 `x`.
        assert_eq!(filename_dir(&"x".repeat(200), 1024).as_deref(), Some("221"));
        assert_eq!(filename_dir("wu_1_0", 0), None);
    }

    #[test]
    fn short_name_gives_make_project_urls() {
        let s = layout(1024);
        assert_eq!(s.scheduler_path(), "/testproj_cgi/cgi");
        assert_eq!(s.upload_path(), "/testproj_cgi/file_upload_handler");
        assert_eq!(s.download_prefix, "/testproj/download");
        assert_eq!(s.page_prefix, "/testproj");
        assert!(s.advertise);
        assert_eq!(
            s.scheduler_url("https://example.org"),
            "https://example.org/testproj_cgi/cgi"
        );
        assert_eq!(
            s.upload_url("https://example.org"),
            "https://example.org/testproj_cgi/file_upload_handler"
        );
        assert_eq!(
            s.page_base("https://example.org"),
            "https://example.org/testproj/"
        );
    }

    #[test]
    fn explicit_prefixes_override_the_short_name() {
        let s = StockSpec {
            short_name: "testproj".into(),
            download_prefix: "/testproj_download/".into(),
            page_prefix: Some("/".into()),
            advertise: Some(false),
            ..Default::default()
        }
        .resolve(0)
        .unwrap()
        .unwrap();
        assert_eq!(s.download_prefix, "/testproj_download");
        assert_eq!(s.cgi_prefix, "/testproj_cgi");
        assert_eq!(s.page_prefix, "");
        assert!(!s.advertise);
        // Without a short name every prefix but the page prefix is needed.
        let e = StockSpec {
            cgi_prefix: "/x_cgi".into(),
            ..Default::default()
        }
        .resolve(0)
        .unwrap_err();
        assert!(e.to_string().contains("stock_short_name"), "{e}");
    }

    /// A project whose `config.xml` puts the three groups of URLs in separate
    /// directories, for instance a download directory next to, not under, the
    /// pages.
    #[test]
    fn each_prefix_can_be_named() {
        let s = StockSpec {
            short_name: "testproj".into(),
            cgi_prefix: "/cgi-bin/testproj".into(),
            upload_prefix: "/testproj_up".into(),
            download_prefix: "/testproj_download".into(),
            ..Default::default()
        }
        .resolve(16)
        .unwrap()
        .unwrap();
        assert_eq!(s.scheduler_path(), "/cgi-bin/testproj/cgi");
        assert_eq!(s.upload_path(), "/testproj_up/file_upload_handler");
        assert_eq!(s.page_prefix, "/testproj");
        assert!(s.is_scheduler_path("/cgi-bin/testproj/cgi"));
        assert!(!s.is_scheduler_path("/testproj_cgi/cgi"));
        assert!(s.is_upload_path("/testproj_up/file_upload_handler"));
        assert!(!s.is_upload_path("/cgi-bin/testproj/file_upload_handler"));
        assert_eq!(
            s.download_name("/testproj_download/9/wu_1_0"),
            Some("wu_1_0")
        );
        assert_eq!(s.download_name("/testproj/download/9/wu_1_0"), None);
        assert_eq!(
            route_files("/testproj_download/9/wu_1_0", Some(&s)),
            FilesRoute::Download("wu_1_0")
        );
        assert_eq!(
            route_files("/testproj_up/file_upload_handler", Some(&s)),
            FilesRoute::Upload
        );
        assert!(route_scheduler("/cgi-bin/testproj/cgi", Some(&s)));
        // The download directory is not a page.
        assert_eq!(
            route_web("/testproj_download/9/wu_1_0", Some(&s)),
            WebRoute::Page {
                path: "/testproj_download/9/wu_1_0",
                prefixed: false
            }
        );
    }

    /// With the default `make_project` layout the download directory is
    /// below the page prefix, so the web Worker's `/*` route would also match
    /// it; the files Worker's more specific route takes it. If a request
    /// reaches the web Worker anyway it is a 404, never a page.
    #[test]
    fn the_default_download_prefix_is_under_the_page_prefix() {
        let s = layout(1024);
        assert!(s
            .download_prefix
            .starts_with(&format!("{}/", s.page_prefix)));
        assert_eq!(
            route_web("/testproj/download/319/wu_1_0", Some(&s)),
            WebRoute::Page {
                path: "/download/319/wu_1_0",
                prefixed: true
            }
        );
    }

    #[test]
    fn no_stock_tag_is_no_layout() {
        assert_eq!(StockSpec::default().resolve(1024).unwrap(), None);
    }

    #[test]
    fn bad_settings_are_refused() {
        for bad in ["a b", "a/b", "../x", "a.b%"] {
            assert!(spec(bad).resolve(0).is_err(), "{bad}");
        }
        let with = |f: &dyn Fn(&mut StockSpec)| {
            let mut s = spec("testproj");
            f(&mut s);
            s.resolve(0)
        };
        assert!(with(&|s| s.cgi_prefix = "no-slash".into()).is_err());
        assert!(with(&|s| s.cgi_prefix = "/a/../b".into()).is_err());
        assert!(with(&|s| s.cgi_prefix = "/a//b".into()).is_err());
        assert!(with(&|s| s.cgi_prefix = "/a?x=1".into()).is_err());
        assert!(with(&|s| s.cgi_prefix = "/sched_cgi".into()).is_ok());
        assert!(with(&|s| s.cgi_prefix = "/sched/cgi".into()).is_err());
        assert!(with(&|s| s.page_prefix = Some("/download".into())).is_err());
        assert!(with(&|s| s.page_prefix = Some("/testproj_cgi".into())).is_err());
        assert!(spec("testproj").resolve(-1).is_err());
    }

    /// Every pair of prefixes the layout names: equal prefixes are refused,
    /// except the cgi and upload prefixes, which are one by default; nested
    /// ones are accepted because the longer route is the more specific.
    #[test]
    fn prefix_pairs_are_refused_or_routed() {
        #[derive(Clone, Copy)]
        enum P {
            Page,
            Cgi,
            Upload,
            Download,
        }
        let set = |s: &mut StockSpec, which: P, v: &str| match which {
            P::Page => s.page_prefix = Some(v.into()),
            P::Cgi => s.cgi_prefix = v.into(),
            P::Upload => s.upload_prefix = v.into(),
            P::Download => s.download_prefix = v.into(),
        };
        // (first prefix, second prefix, accepted when both are set to the same path)
        let pairs = [
            (P::Page, P::Cgi, false),
            (P::Page, P::Upload, false),
            (P::Page, P::Download, false),
            (P::Cgi, P::Download, false),
            (P::Upload, P::Download, false),
            (P::Cgi, P::Upload, true),
        ];
        for (a, b, same_ok) in pairs {
            let mut s = spec("testproj");
            set(&mut s, a, "/shared");
            set(&mut s, b, "/shared");
            assert_eq!(s.resolve(0).is_ok(), same_ok);
            // One nested in the other is accepted, either way round.
            for (outer, inner) in [("/shared", "/shared/sub"), ("/shared/sub", "/shared")] {
                let mut s = spec("testproj");
                set(&mut s, a, outer);
                set(&mut s, b, inner);
                assert!(s.resolve(0).is_ok(), "{outer} and {inner}");
            }
        }
        // The page prefix may be empty next to anything.
        let mut s = spec("testproj");
        s.page_prefix = Some(String::new());
        s.download_prefix = "/shared".into();
        assert!(s.resolve(0).is_ok());
        // Nothing of the layout may sit under /admin/, and the native
        // roots themselves are not page or cgi prefixes.
        for (which, v, ok) in [
            (P::Download, "/admin/files", false),
            (P::Download, "/admin2", true),
            (P::Download, "/download", true),
            (P::Download, "/downloads", true),
            (P::Page, "/admin", false),
            (P::Page, "/admin2", true),
            (P::Page, "/scheduler", true),
            (P::Page, "/uploadtest", true),
            (P::Page, "/sched", false),
            (P::Page, "/upload", false),
            (P::Cgi, "/upload", false),
            (P::Upload, "/admin", false),
        ] {
            let mut s = spec("testproj");
            set(&mut s, which, v);
            assert_eq!(s.resolve(0).is_ok(), ok, "{v}");
        }
    }

    /// A model of how Cloudflare picks a route: among the patterns that match
    /// the request's path and query string (a pattern is matched against the
    /// URL after the host, so a query string is part of what a wildcard must
    /// cover), the one with the longest literal part, and an exact pattern
    /// before a wildcard one of the same length. `None` when the best two
    /// patterns tie and name different Workers.
    fn pick(routes: &[(String, RouteTarget)], path: &str) -> Option<RouteTarget> {
        let mut best: Option<(usize, bool, RouteTarget)> = None;
        let mut tie = false;
        for (pattern, target) in routes {
            let (literal, wildcard) = match pattern.strip_suffix('*') {
                Some(l) => (l, true),
                None => (pattern.as_str(), false),
            };
            let matches = if wildcard {
                path.starts_with(literal)
            } else {
                path == literal
            };
            if !matches {
                continue;
            }
            let key = (literal.len(), !wildcard);
            match best {
                Some((len, exact, t)) if (len, exact) == key => tie |= t != *target,
                Some((len, exact, _)) if (len, exact) > key => {}
                _ => {
                    best = Some((key.0, key.1, *target));
                    tie = false;
                }
            }
        }
        if tie {
            None
        } else {
            best.map(|(_, _, t)| t)
        }
    }

    fn with_native(s: &StockLayout) -> Vec<(String, RouteTarget)> {
        let mut routes: Vec<(String, RouteTarget)> = NATIVE_ROUTES
            .iter()
            .map(|(p, t)| (p.to_string(), *t))
            .collect();
        routes.extend(s.routes());
        routes
    }

    /// The URLs of a layout and of the native layout, each with and without a
    /// query string, and the Worker that has to answer them.
    fn expected(s: &StockLayout) -> Vec<(String, RouteTarget)> {
        use RouteTarget::*;
        let mut urls = vec![
            (s.scheduler_path(), Scheduler),
            (s.upload_path(), Files),
            (
                format!(
                    "{}/{}",
                    s.download_prefix,
                    download_tail("wu_1_0", s.fanout)
                ),
                Files,
            ),
            ("/sched".to_string(), Scheduler),
            ("/upload/file_upload_handler".to_string(), Files),
            ("/download/wu_1_0".to_string(), Files),
            ("/admin/v1/status".to_string(), Admin),
            ("/".to_string(), Web),
            ("/home.php".to_string(), Web),
            ("/get_project_config.php".to_string(), Web),
        ];
        if !s.page_prefix.is_empty() {
            for page in ["", "/", "/home.php", "/get_project_config.php"] {
                urls.push((format!("{}{page}", s.page_prefix), Web));
            }
        }
        let with_query: Vec<_> = urls.iter().map(|(u, t)| (format!("{u}?x=1"), *t)).collect();
        urls.extend(with_query);
        urls
    }

    /// Every combination of prefixes from a pool that has the cases that
    /// overlap (a prefix equal to, under, or beginning with another, and
    /// the endpoint paths themselves): each one the validation accepts routes
    /// every one of its URLs, with and without a query string, to the right
    /// Worker, with the routes of `StockLayout::routes` next to the native
    /// ones. A combination that does not is a rule the validation lacks.
    #[test]
    fn every_accepted_layout_routes_its_urls() {
        let pages = [
            None,
            Some("/x"),
            Some("/x/cgi"),
            Some("/x/cgi/sub"),
            Some("/x_cgi"),
            Some("/x/download"),
            Some("/x/file_upload_handler"),
            Some("/x/file_upload_handler/sub"),
            Some("/a"),
            Some("/scheduler"),
            Some("/x/c"),
        ];
        let dirs = [
            "/x",
            "/x/cgi",
            "/x_cgi",
            "/x/download",
            "/a",
            "/a/download",
            "/x/cgi/sub",
            "/x/file_upload_handler",
            "/xx",
        ];
        let (mut accepted, mut refused) = (0, 0);
        for page in pages {
            for cgi in dirs {
                for upload in dirs {
                    for download in dirs {
                        for fanout in [0, 16] {
                            let sp = StockSpec {
                                short_name: "t".into(),
                                cgi_prefix: cgi.into(),
                                upload_prefix: upload.into(),
                                download_prefix: download.into(),
                                page_prefix: Some(page.unwrap_or("").into()),
                                ..Default::default()
                            };
                            let Ok(Some(s)) = sp.resolve(fanout) else {
                                refused += 1;
                                continue;
                            };
                            accepted += 1;
                            let routes = with_native(&s);
                            for (url, want) in expected(&s) {
                                assert_eq!(
                                    pick(&routes, &url),
                                    Some(want),
                                    "{url} with page {page:?}, cgi {cgi}, upload {upload}, \
                                     download {download}"
                                );
                            }
                        }
                    }
                }
            }
        }
        // The pool has both kinds.
        assert!(accepted > 1000 && refused > 1000, "{accepted} {refused}");
    }

    /// The endpoint collisions: a page prefix that is, or lies under, the
    /// scheduler path or the upload handler path, and equal endpoint paths.
    #[test]
    fn endpoint_paths_cannot_collide_with_prefixes() {
        let mk = |page: &str, cgi: &str, upload: &str, download: &str| StockSpec {
            short_name: "t".into(),
            page_prefix: Some(page.into()),
            cgi_prefix: cgi.into(),
            upload_prefix: upload.into(),
            download_prefix: download.into(),
            ..Default::default()
        };
        // Refused: the page prefix is the scheduler path, or the completed
        // upload handler path, or under either; a download prefix or another
        // directory is an endpoint path.
        for (page, cgi, upload, download) in [
            ("/x/cgi", "/x", "/u", "/d"),
            ("/x/cgi/sub", "/x", "/u", "/d"),
            ("/x/file_upload_handler", "/c", "/x", "/d"),
            ("/x/file_upload_handler/p", "/c", "/x", "/d"),
            ("/p", "/x", "/u", "/x/cgi"),
            ("/p", "/c", "/x", "/x/file_upload_handler"),
            ("/p", "/c", "/c/cgi", "/d"),
            ("/p", "/c", "/u", "/c/cgi"),
        ] {
            assert!(
                mk(page, cgi, upload, download).resolve(0).is_err(),
                "{page} {cgi} {upload} {download}"
            );
        }
        // Accepted: the same paths one level apart, the other way round.
        for (page, cgi, upload, download) in [
            ("/x", "/x_cgi", "/x_cgi", "/d"),
            ("/x_cgi", "/x", "/u", "/d"),
            ("/x/c", "/x", "/u", "/d"),
            ("/p", "/x/cgi", "/x/cgi", "/d"),
            ("/a", "/x", "/u", "/a/download"),
        ] {
            assert!(
                mk(page, cgi, upload, download).resolve(0).is_ok(),
                "{page} {cgi} {upload} {download}"
            );
        }
    }

    /// Short names that begin with a native prefix, or are one without being
    /// reserved, with the routes of the layout next to the native ones: every
    /// stock URL and every native URL reaches the right Worker.
    #[test]
    fn stock_routes_beat_the_native_ones_they_overlap() {
        let shorts = [
            "example",
            "scheduler",
            "sched_x",
            "uploadtest",
            "upload2",
            "downloads",
            "admin2",
            "a",
        ];
        for short in shorts {
            for (extra_download, fanout) in [(None, 16), (Some(format!("/{short}_files")), 0)] {
                let mut sp = spec(short);
                if let Some(d) = &extra_download {
                    sp.download_prefix = d.clone();
                }
                let s = sp.resolve(fanout).unwrap().unwrap();
                let routes = with_native(&s);
                for (path, want) in expected(&s) {
                    assert_eq!(pick(&routes, &path), Some(want), "{short}: {path}");
                }
                // Without the layout's page routes the native ones capture
                // the pages of a short name that begins with `sched` or
                // `upload`, which is why they are listed.
                let page = format!("{}/home.php", s.page_prefix);
                let native: Vec<_> = NATIVE_ROUTES
                    .iter()
                    .map(|(p, t)| (p.to_string(), *t))
                    .collect();
                let captured = short.starts_with("sched") || short.starts_with("upload");
                assert_eq!(
                    pick(&native, &page) != Some(RouteTarget::Web),
                    captured,
                    "{short}"
                );
            }
        }
    }

    #[test]
    fn the_scheduler_worker_answers_only_its_own_paths() {
        let s = layout(16);
        // The route `/sched*` also delivers these; they are not scheduler requests.
        for path in ["/scheduler/home.php", "/sched_x", "/schedule", "/sched/x"] {
            assert!(!route_scheduler(path, Some(&s)), "{path}");
        }
        assert!(route_scheduler("/sched", None));
    }

    #[test]
    fn downloads_map_to_the_stored_name() {
        let s = layout(1024);
        assert_eq!(
            s.download_name("/testproj/download/319/wu_1_0"),
            Some("wu_1_0")
        );
        // A wrong directory, a missing one, an extra one, another prefix.
        assert_eq!(s.download_name("/testproj/download/318/wu_1_0"), None);
        assert_eq!(s.download_name("/testproj/download/wu_1_0"), None);
        assert_eq!(s.download_name("/testproj/download/319/x/wu_1_0"), None);
        assert_eq!(s.download_name("/testproj/download/319/"), None);
        assert_eq!(s.download_name("/testproj/download/"), None);
        assert_eq!(s.download_name("/testproj/download"), None);
        assert_eq!(s.download_name("/testproj/downloads/319/wu_1_0"), None);
        assert_eq!(s.download_name("/other/download/319/wu_1_0"), None);
        // The directory is lowercase hex without padding.
        assert_eq!(s.download_name("/testproj/download/0319/wu_1_0"), None);
        assert_eq!(s.download_name("/testproj/download/84/0"), Some("0"));
        let flat = layout(0);
        assert_eq!(
            flat.download_name("/testproj/download/wu_1_0"),
            Some("wu_1_0")
        );
        assert_eq!(flat.download_name("/testproj/download/319/wu_1_0"), None);
        assert_eq!(flat.download_name("/testproj/download/"), None);
    }

    #[test]
    fn file_urls_include_the_directory() {
        let urls = FileUrls {
            base: "https://example.org/testproj/download".into(),
            fanout: 1024,
        };
        assert_eq!(
            urls.download("wu_1_0"),
            "https://example.org/testproj/download/319/wu_1_0"
        );
        let flat = FileUrls {
            base: "https://example.org/download".into(),
            fanout: 0,
        };
        assert_eq!(
            flat.download("wu_1_0"),
            "https://example.org/download/wu_1_0"
        );
    }

    #[test]
    fn files_routes() {
        let s = layout(1024);
        let r = |p, stock| route_files(p, stock);
        // Native paths need no setting.
        assert_eq!(r("/download/a", None), FilesRoute::Download("a"));
        assert_eq!(r("/upload/file_upload_handler", None), FilesRoute::Upload);
        assert_eq!(r("/upload/other", None), FilesRoute::NotFound);
        assert_eq!(
            r("/testproj_cgi/file_upload_handler", None),
            FilesRoute::NotFound
        );
        assert_eq!(r("/testproj/download/1b9/a", None), FilesRoute::NotFound);
        // With the layout the stock paths answer too.
        assert_eq!(
            r("/testproj_cgi/file_upload_handler", Some(&s)),
            FilesRoute::Upload
        );
        assert_eq!(
            r("/testproj/download/1b9/a", Some(&s)),
            FilesRoute::Download("a")
        );
        assert_eq!(r("/download/a", Some(&s)), FilesRoute::Download("a"));
        assert_eq!(
            r("/testproj/download/000/a", Some(&s)),
            FilesRoute::NotFound
        );
        assert_eq!(r("/testproj_cgi/cgi", Some(&s)), FilesRoute::NotFound);
        assert_eq!(
            r("/testproj_cgi/file_upload_handler/x", Some(&s)),
            FilesRoute::NotFound
        );
    }

    /// A stock download prefix of `/download`, as a project whose `download_url`
    /// was the host's `/download` directory would have.
    #[test]
    fn a_stock_prefix_of_download_is_reached_by_nested_paths() {
        let s = StockSpec {
            short_name: "testproj".into(),
            download_prefix: "/download".into(),
            ..Default::default()
        }
        .resolve(1024)
        .unwrap()
        .unwrap();
        // Without the layout loaded the nested path is not a native name.
        assert_eq!(
            route_files("/download/319/wu_1_0", None),
            FilesRoute::NotFound
        );
        assert_eq!(
            route_files("/download/319/wu_1_0", Some(&s)),
            FilesRoute::Download("wu_1_0")
        );
        assert_eq!(
            route_files("/download/wu_1_0", Some(&s)),
            FilesRoute::Download("wu_1_0")
        );
        assert_eq!(
            route_files("/download/318/wu_1_0", Some(&s)),
            FilesRoute::NotFound
        );
    }

    #[test]
    fn scheduler_routes() {
        let s = layout(1024);
        assert!(route_scheduler("/sched", None));
        assert!(!route_scheduler("/testproj_cgi/cgi", None));
        assert!(route_scheduler("/testproj_cgi/cgi", Some(&s)));
        assert!(!route_scheduler("/testproj_cgi/cgi/x", Some(&s)));
        assert!(!route_scheduler(
            "/testproj_cgi/file_upload_handler",
            Some(&s)
        ));
        assert!(!route_scheduler("/testproj_cgi", Some(&s)));
        assert!(!route_scheduler("/", Some(&s)));
    }

    #[test]
    fn web_routes() {
        let s = layout(0);
        let page = |path, prefixed| WebRoute::Page { path, prefixed };
        assert_eq!(route_web("/home.php", None), page("/home.php", false));
        assert_eq!(
            route_web("/testproj/home.php", None),
            page("/testproj/home.php", false)
        );
        assert_eq!(route_web("/home.php", Some(&s)), page("/home.php", false));
        assert_eq!(route_web("/", Some(&s)), page("/", false));
        assert_eq!(
            route_web("/testproj/home.php", Some(&s)),
            page("/home.php", true)
        );
        assert_eq!(route_web("/testproj/", Some(&s)), page("/", true));
        assert_eq!(
            route_web("/testproj", Some(&s)),
            WebRoute::Redirect("/testproj/".into())
        );
        // Only whole segments count as the prefix.
        assert_eq!(
            route_web("/testproj2/home.php", Some(&s)),
            page("/testproj2/home.php", false)
        );
        // No page prefix: everything is at the root.
        let no_pages = StockSpec {
            short_name: "testproj".into(),
            page_prefix: Some(String::new()),
            ..Default::default()
        }
        .resolve(0)
        .unwrap()
        .unwrap();
        assert_eq!(
            route_web("/testproj/home.php", Some(&no_pages)),
            page("/testproj/home.php", false)
        );
    }

    #[test]
    fn url_helpers() {
        assert_eq!(
            url_origin("https://example.org/a/b"),
            Some("https://example.org")
        );
        assert_eq!(
            url_origin("https://example.org"),
            Some("https://example.org")
        );
        assert_eq!(
            url_origin("http://example.org:8080/"),
            Some("http://example.org:8080")
        );
        assert_eq!(url_origin("example.org/a"), None);
        assert_eq!(url_path("https://example.org/testproj/"), "/testproj");
        assert_eq!(url_path("https://example.org"), "");
        assert_eq!(
            comparable_url("HTTP://Example.ORG/Testproj/download/a"),
            "example.org/Testproj/download/a"
        );
    }
}
