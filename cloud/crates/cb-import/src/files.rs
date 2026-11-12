//! Files the imported documents refer to: the `job_file` rows and the
//! manifest of files to copy to the bucket.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use cb_core::stock::{comparable_url, filename_dir, url_origin};
use cb_crypto::Md5Stream;
use cb_proto::xml::XmlParser;

use crate::error::{show_ids, ImportError, Result};
use crate::schema::TableInfo;
use crate::summary::Summary;
use crate::value::Value;

/// One `<file_info>` of a stored document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DocFile {
    pub name: String,
    pub url: Option<String>,
    pub md5: Option<String>,
    pub nbytes: Option<u64>,
    pub no_delete: bool,
    pub sticky: bool,
    pub signature: Option<String>,
    /// The client computes it and uploads it (`generated_locally`,
    /// `upload_when_present` or `max_nbytes`).
    pub is_output: bool,
}

/// The `<file_info>` elements of an `xml_doc`, `xml_doc_in` or `xml_doc_out`.
pub fn doc_files(xml: &str) -> Vec<DocFile> {
    let mut v = Vec::new();
    let mut xp = XmlParser::new(xml);
    while xp.get_tag() {
        if !xp.is_tag() || !xp.match_tag("file_info") {
            continue;
        }
        let mut f = DocFile::default();
        while xp.get_tag() {
            if !xp.is_tag() {
                continue;
            }
            if xp.match_tag("/file_info") {
                break;
            }
            if let Some(n) = xp.parse_str("name") {
                f.name = n;
            } else if let Some(u) = xp.parse_str("url") {
                if f.url.is_none() {
                    f.url = Some(u);
                }
            } else if let Some(m) = xp.parse_str("md5_cksum") {
                f.md5 = Some(m.trim().to_ascii_lowercase());
            } else if let Some(n) = xp.parse_double("nbytes") {
                f.nbytes = Some(n as u64);
            } else if let Some(n) = xp.parse_double("max_nbytes") {
                if n > 0.0 {
                    f.is_output = true;
                }
            } else if let Some(b) = xp.parse_bool("no_delete") {
                f.no_delete = b;
            } else if let Some(b) = xp.parse_bool("sticky") {
                f.sticky = b;
            } else if let Some(b) = xp.parse_bool("generated_locally") {
                f.is_output |= b;
            } else if let Some(b) = xp.parse_bool("upload_when_present") {
                f.is_output |= b;
            } else if let Some(s) = xp.parse_str("file_signature") {
                f.signature = Some(s);
            } else {
                xp.skip_unexpected();
            }
        }
        if !f.name.is_empty() {
            v.push(f);
        }
    }
    v
}

/// Where the stock server keeps a file of `dir`: `update_versions` puts an
/// application file directly in the download directory, while `create_work`
/// and the upload handler use the hash directory (`dir_hier_path`). Looks
/// in the hash directory first, then at the top.
pub fn find_in_dir(dir: &Path, name: &str, fanout: u32) -> Option<PathBuf> {
    let hashed = filename_dir(name, fanout).map(|d| dir.join(d).join(name));
    if let Some(p) = hashed {
        if p.is_file() {
            return Some(p);
        }
    }
    let flat = dir.join(name);
    flat.is_file().then_some(flat)
}

/// Size and MD5 of a file, read in 1 MiB pieces.
pub fn size_and_md5(path: &Path) -> Option<(u64, String)> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut m = Md5Stream::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        m.update(&buf[..n]);
    }
    let n = m.nbytes();
    Some((n, m.finalize_hex()))
}

#[derive(Debug, Clone)]
struct Download {
    nbytes: Option<u64>,
    md5: Option<String>,
    no_delete: bool,
    create_time: i64,
}

#[derive(Debug, Clone)]
struct Upload {
    nbytes: Option<u64>,
    md5: Option<String>,
    /// The result is received and its output is not deleted: the file must exist.
    must_exist: bool,
}

pub struct FileCollector {
    fanout: u32,
    download_dir: Option<PathBuf>,
    upload_dir: Option<PathBuf>,
    project_dir: Option<PathBuf>,
    own_origins: Vec<String>,
    downloads: BTreeMap<String, Download>,
    uploads: BTreeMap<String, Upload>,
    /// File names whose documents disagree about size or MD5.
    pub conflicts: Vec<String>,
    /// Names refused because they are not flat file names (a path, `..`, an
    /// absolute path). Document text is client-supplied and may hold anything.
    pub refused_names: u64,
}

impl FileCollector {
    pub fn new(
        fanout: u32,
        project_dir: Option<PathBuf>,
        download_dir: Option<PathBuf>,
        upload_dir: Option<PathBuf>,
        own_origins: Vec<String>,
    ) -> Self {
        let download_dir =
            download_dir.or_else(|| project_dir.as_ref().map(|p| p.join("download")));
        let upload_dir = upload_dir.or_else(|| project_dir.as_ref().map(|p| p.join("upload")));
        FileCollector {
            fanout,
            download_dir,
            upload_dir,
            project_dir,
            own_origins: own_origins.iter().map(|o| comparable_url(o)).collect(),
            downloads: BTreeMap::new(),
            uploads: BTreeMap::new(),
            conflicts: Vec::new(),
            refused_names: 0,
        }
    }

    fn is_staged(&self, f: &DocFile) -> bool {
        if self.own_origins.is_empty() {
            return true;
        }
        match f.url.as_deref().and_then(url_origin) {
            Some(o) => self.own_origins.contains(&comparable_url(o)),
            None => false,
        }
    }

    /// Checked with the rule the server applies to a name it stores (`is_flat_name`), before
    /// any name is looked up on disk or written to the manifest.
    fn name_ok(&mut self, name: &str) -> bool {
        if cb_core::upload::is_flat_name(name) {
            true
        } else {
            self.refused_names += 1;
            false
        }
    }

    fn add_download(&mut self, f: &DocFile, create_time: i64, force_keep: bool) {
        if f.is_output || !self.is_staged(f) || !self.name_ok(&f.name) {
            return;
        }
        let keep = force_keep || f.no_delete;
        match self.downloads.get_mut(&f.name) {
            Some(d) => {
                d.no_delete |= keep;
                if (d.md5.is_some() && f.md5.is_some() && d.md5 != f.md5)
                    || (d.nbytes.is_some() && f.nbytes.is_some() && d.nbytes != f.nbytes)
                {
                    self.conflicts.push(f.name.clone());
                }
                if d.md5.is_none() {
                    d.md5 = f.md5.clone();
                }
                if d.nbytes.is_none() {
                    d.nbytes = f.nbytes;
                }
                d.create_time = d.create_time.min(create_time);
            }
            None => {
                self.downloads.insert(
                    f.name.clone(),
                    Download {
                        nbytes: f.nbytes,
                        md5: f.md5.clone(),
                        no_delete: keep,
                        create_time,
                    },
                );
            }
        }
    }

    /// Files of an app version document: kept until the version is removed.
    pub fn app_version(&mut self, create_time: i64, xml: &str) {
        for f in doc_files(xml) {
            self.add_download(&f, create_time, true);
        }
    }

    pub fn workunit(&mut self, create_time: i64, xml: &str) {
        for f in doc_files(xml) {
            self.add_download(&f, create_time, false);
        }
    }

    /// Output files of a result. `server_state` 4 is in progress, 5 over;
    /// `outcome` 1 is success; `file_delete_state` 2 is done (files deleted).
    pub fn result(
        &mut self,
        server_state: i64,
        outcome: i64,
        file_delete_state: i64,
        xml_in: &str,
        xml_out: &str,
    ) {
        let received = server_state == 5 && outcome == 1 && file_delete_state != 2;
        let in_progress = server_state == 4;
        if !received && !in_progress {
            return;
        }
        let mut docs = doc_files(xml_out);
        if docs.is_empty() {
            docs = doc_files(xml_in);
        }
        for f in docs {
            if !f.is_output && f.url.is_some() && f.md5.is_none() {
                continue;
            }
            if !self.name_ok(&f.name) {
                continue;
            }
            let e = self.uploads.entry(f.name.clone()).or_insert(Upload {
                nbytes: f.nbytes,
                md5: f.md5.clone(),
                must_exist: received,
            });
            e.must_exist |= received;
        }
    }

    pub fn download_count(&self) -> usize {
        self.downloads.len()
    }

    /// `job_file` rows in the column order of `t`.
    pub fn job_file_rows(&self, t: &TableInfo, summary: &mut Summary) -> Vec<Vec<Value>> {
        let mut rows = Vec::new();
        let mut incomplete = 0u64;
        for (name, d) in &self.downloads {
            let (mut nbytes, mut md5) = (d.nbytes, d.md5.clone());
            if (nbytes.is_none() || md5.is_none()) && self.download_dir.is_some() {
                if let Some((n, m)) = self.hash_file(&self.download_dir, name) {
                    nbytes = nbytes.or(Some(n));
                    md5 = md5.or(Some(m));
                }
            }
            let (Some(nbytes), Some(md5)) = (nbytes, md5) else {
                incomplete += 1;
                continue;
            };
            let mut row: Vec<Value> = (0..t.cols.len()).map(|i| t.default_of(i)).collect();
            let set = |row: &mut Vec<Value>, c: &str, v: Value| {
                if let Some(i) = t.index(c) {
                    row[i] = v;
                }
            };
            set(&mut row, "name", Value::Text(name.clone()));
            set(&mut row, "create_time", Value::Int(d.create_time));
            set(&mut row, "nbytes", Value::Int(nbytes as i64));
            set(&mut row, "md5", Value::Text(md5));
            set(&mut row, "no_delete", Value::Int(d.no_delete as i64));
            rows.push(row);
        }
        if incomplete > 0 {
            summary.warn(format!(
                "{incomplete} staged files have no size or MD5 in their documents and no file to read them from; they get no job_file row"
            ));
        }
        rows
    }

    fn hash_file(&self, dir: &Option<PathBuf>, name: &str) -> Option<(u64, String)> {
        size_and_md5(&find_in_dir(dir.as_ref()?, name, self.fanout)?)
    }

    fn relative(&self, p: &Path) -> String {
        match &self.project_dir {
            Some(root) => p.strip_prefix(root).unwrap_or(p).display().to_string(),
            None => p.display().to_string(),
        }
    }

    /// Writes the manifest: one line for each file to copy, `stock path
    /// (relative to the project directory)`, `bucket key`, `bytes`, `md5`,
    /// separated by tabs. Reads each file that is on disk to get its MD5.
    pub fn write_manifest(&self, out: &Path, list: usize, summary: &mut Summary) -> Result<()> {
        let f = std::fs::File::create(out)
            .map_err(|_| ImportError::usage("cannot create the --files-manifest file"))?;
        let mut w = std::io::BufWriter::new(f);
        let mut missing: Vec<String> = Vec::new();
        let mut missing_uploads = 0u64;
        let mut bad_md5: Vec<String> = Vec::new();
        let mut bad_size: Vec<String> = Vec::new();
        let (mut n_down, mut n_up, mut bytes) = (0u64, 0u64, 0u64);
        let werr = |_| ImportError::target("writing the manifest failed");
        for (name, d) in &self.downloads {
            let entry = self.manifest_entry(
                &self.download_dir,
                "download",
                name,
                d.nbytes,
                d.md5.as_deref(),
            );
            match entry {
                Entry::Ok {
                    line,
                    nbytes,
                    md5_differs,
                    size_differs,
                } => {
                    if md5_differs {
                        bad_md5.push(name.clone());
                    }
                    if size_differs {
                        bad_size.push(name.clone());
                    }
                    writeln!(w, "{line}").map_err(werr)?;
                    n_down += 1;
                    bytes += nbytes;
                }
                Entry::Missing => missing.push(name.clone()),
            }
        }
        for (name, u) in &self.uploads {
            let entry =
                self.manifest_entry(&self.upload_dir, "upload", name, u.nbytes, u.md5.as_deref());
            match entry {
                Entry::Ok {
                    line,
                    nbytes,
                    md5_differs,
                    size_differs,
                } => {
                    if md5_differs {
                        bad_md5.push(name.clone());
                    }
                    if size_differs {
                        bad_size.push(name.clone());
                    }
                    writeln!(w, "{line}").map_err(werr)?;
                    n_up += 1;
                    bytes += nbytes;
                }
                Entry::Missing => {
                    // A result still in progress may not have uploaded yet.
                    if u.must_exist {
                        missing_uploads += 1;
                        missing.push(name.clone());
                    }
                }
            }
        }
        w.flush().map_err(werr)?;
        summary.count("manifest download files", n_down);
        summary.count("manifest upload files", n_up);
        summary.count("manifest bytes", bytes);
        // Names are listed only when asked for (`--list-missing N`).
        let names = |v: &[String]| -> String {
            if list == 0 {
                String::new()
            } else {
                format!(
                    "; first {}: {}",
                    list.min(v.len()),
                    v.iter().take(list).cloned().collect::<Vec<_>>().join(", ")
                )
            }
        };
        if !missing.is_empty() {
            summary.warn(format!(
                "{} files named by the imported documents are not in the stock directories ({} download files, {} uploads of received results){}",
                missing.len(),
                missing.len() as u64 - missing_uploads,
                missing_uploads,
                names(&missing)
            ));
        }
        if !bad_md5.is_empty() {
            summary.warn(format!(
                "{} files have an MD5 that differs from the document's{}",
                bad_md5.len(),
                names(&bad_md5)
            ));
        }
        if !bad_size.is_empty() {
            summary.warn(format!(
                "{} files have a size that differs from the document's{}",
                bad_size.len(),
                names(&bad_size)
            ));
        }
        if !self.conflicts.is_empty() {
            summary.warn(format!(
                "{} files are described with different sizes or MD5s by different documents{}",
                self.conflicts.len(),
                names(&self.conflicts)
            ));
        }
        let _ = show_ids;
        Ok(())
    }

    fn manifest_entry(
        &self,
        dir: &Option<PathBuf>,
        prefix: &str,
        name: &str,
        doc_nbytes: Option<u64>,
        doc_md5: Option<&str>,
    ) -> Entry {
        match dir {
            Some(d) => match self.hash_file(dir, name) {
                Some((n, m)) => Entry::Ok {
                    line: format!(
                        "{}\t{prefix}/{name}\t{n}\t{m}",
                        self.relative(&find_in_dir(d, name, self.fanout).unwrap_or_default())
                    ),
                    nbytes: n,
                    md5_differs: doc_md5.is_some_and(|x| x != m),
                    size_differs: doc_nbytes.is_some_and(|x| x != n),
                },
                None => Entry::Missing,
            },
            None => {
                // No directory to read: the documents' figures, the path the
                // stock server would have used.
                let rel = match filename_dir(name, self.fanout) {
                    Some(d) => format!("{prefix}/{d}/{name}"),
                    None => format!("{prefix}/{name}"),
                };
                match (doc_nbytes, doc_md5) {
                    (Some(n), Some(m)) => Entry::Ok {
                        line: format!("{rel}\t{prefix}/{name}\t{n}\t{m}"),
                        nbytes: n,
                        md5_differs: false,
                        size_differs: false,
                    },
                    _ => Entry::Missing,
                }
            }
        }
    }
}

enum Entry {
    Ok {
        line: String,
        nbytes: u64,
        md5_differs: bool,
        size_differs: bool,
    },
    Missing,
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP_DOC: &str = "<file_info>\n    <name>app_1.0_x86_64</name>\n    <url>http://p.test/proj/download/1f/app_1.0_x86_64</url>\n    <executable/>\n    <md5_cksum>0123456789abcdef0123456789abcdef</md5_cksum>\n    <nbytes>1234</nbytes>\n    <file_signature>\nAA\nBB\n.\n    </file_signature>\n</file_info>\n<app_version>\n</app_version>\n";

    #[test]
    fn reads_file_info() {
        let f = doc_files(APP_DOC);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].name, "app_1.0_x86_64");
        assert_eq!(f[0].nbytes, Some(1234));
        assert_eq!(
            f[0].md5.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert!(f[0].signature.as_deref().unwrap().contains("AA"));
        assert!(!f[0].is_output);
    }

    #[test]
    fn output_file_infos() {
        let xml = "<file_info>\n<name>r_0</name>\n<generated_locally/>\n<upload_when_present/>\n<max_nbytes>1e6</max_nbytes>\n<url>http://p.test/proj_cgi/file_upload_handler</url>\n</file_info>";
        let f = doc_files(xml);
        assert!(f[0].is_output);
    }

    #[test]
    fn job_file_rows_and_origin_filter() {
        let t = {
            let c = rusqlite::Connection::open_in_memory().unwrap();
            crate::schema::apply_migrations(&c).unwrap();
            crate::schema::table_info(&c, "job_file").unwrap()
        };
        let mut c = FileCollector::new(0, None, None, None, vec!["http://p.test".into()]);
        c.app_version(10, APP_DOC);
        c.workunit(
            20,
            "<file_info><name>in1</name><url>http://p.test/proj/download/in1</url><md5_cksum>aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</md5_cksum><nbytes>5</nbytes></file_info>\
             <file_info><name>ext</name><url>http://elsewhere.test/ext</url><md5_cksum>bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb</md5_cksum><nbytes>6</nbytes></file_info>",
        );
        let mut s = Summary::default();
        let rows = c.job_file_rows(&t, &mut s);
        assert_eq!(rows.len(), 2, "the external file is not staged");
        let ni = t.index("no_delete").unwrap();
        assert_eq!(rows[0][ni], Value::Int(1), "app version files are kept");
        assert_eq!(rows[1][ni], Value::Int(0));
    }
}
