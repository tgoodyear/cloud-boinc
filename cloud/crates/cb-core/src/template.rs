//! Input and output templates: ports of `tools/process_input_template.cpp`
//! and `tools/process_result_template.cpp`, with the bucket in place of the
//! download directory.

use std::collections::HashMap;

use cb_crypto::Signer;
use cb_proto::xml::{line, strip_whitespace, XmlParser};

use crate::model::Workunit;
use crate::stock::FileUrls;
use crate::{CoreError, CoreResult};

const OUTFILE_MACRO: &str = "<OUTFILE_";
const UPLOAD_URL_MACRO: &str = "<UPLOAD_URL/>";

fn xml_err(msg: impl Into<String>) -> CoreError {
    CoreError::Xml(msg.into())
}

/// Appends an upload certificate to every `<file_info>` of a result
/// document (`add_signatures`).
fn add_signatures(xml: &str, signer: &Signer) -> CoreResult<String> {
    const START: &str = "<file_info>\n";
    const END: &str = "</file_info>";
    let mut out = String::with_capacity(xml.len() + 512);
    let mut rest = xml;
    while let Some(i) = rest.find(START) {
        let body_start = i + START.len();
        let body_len = rest[body_start..]
            .find(END)
            .ok_or_else(|| xml_err("output template: <file_info> is not closed"))?;
        let body = &rest[body_start..body_start + body_len];
        let name = line::parse_str(body, "<name>")
            .ok_or_else(|| xml_err("output template: <file_info> without <name>"))?;
        let max_nbytes = line::parse_double(body, "<max_nbytes>")
            .ok_or_else(|| xml_err("output template: <file_info> without <max_nbytes>"))?;
        let signature = signer.sign_upload_certificate(&name, max_nbytes)?;
        out.push_str(&rest[..body_start + body_len]);
        out.push_str("<xml_signature>\n");
        out.push_str(&signature);
        out.push_str("</xml_signature>\n");
        rest = &rest[body_start + body_len..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Turns an output template into the `xml_doc_in` of one result
/// (`process_result_template`): `<OUTFILE_n/>` becomes `base_filename`
/// followed by `n`, `<UPLOAD_URL/>` becomes `upload_url`, and each
/// `<file_info>` gets an upload certificate when `signer` is given.
pub fn process_result_template(
    template: &str,
    signer: Option<&Signer>,
    base_filename: &str,
    upload_url: &str,
) -> CoreResult<String> {
    let mut doc = String::with_capacity(template.len() + 256);
    let mut rest = template;
    while let Some(i) = rest.find(OUTFILE_MACRO) {
        let after = &rest[i + OUTFILE_MACRO.len()..];
        let end = after
            .find("/>")
            .ok_or_else(|| xml_err("output template: unterminated <OUTFILE_"))?;
        if after.find('>') != Some(end + 1) {
            return Err(xml_err("output template: malformed <OUTFILE_ macro"));
        }
        doc.push_str(&rest[..i]);
        doc.push_str(base_filename);
        doc.push_str(&after[..end]);
        rest = &after[end + 2..];
    }
    doc.push_str(rest);
    let doc = doc.replace(UPLOAD_URL_MACRO, upload_url);
    let doc = match signer {
        Some(s) => add_signatures(&doc, s)?,
        None => doc,
    };
    const OPEN: &str = "<output_template>";
    let body = match doc.find(OPEN) {
        // Upstream drops the first strlen(OPEN) characters wherever the tag
        // was found; do what it means instead.
        Some(i) => {
            let inner = &doc[i + OPEN.len()..];
            match inner.find("</output_template>") {
                Some(j) => &inner[..j],
                None => inner,
            }
        }
        None => doc.as_str(),
    };
    Ok(strip_whitespace(body).to_string())
}

/// One variable input file of a job, in the order the input template's
/// `<file_info>` elements use them.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct InputFile {
    pub name: String,
    /// Set for a file served from somewhere else: the workunit then names
    /// it `jf_<md5>` and points at this URL.
    pub url: Option<String>,
    /// Needed with `url`; otherwise taken from the staged file.
    pub md5: String,
    pub nbytes: f64,
}

/// MD5 and size of a file staged under `download/`.
#[derive(Debug, Clone, PartialEq)]
pub struct StagedFile {
    pub md5: String,
    pub nbytes: f64,
    /// Kept when the jobs using it are done; written into the workunit as
    /// `<no_delete/>`, which is what the file deletion step reads.
    pub no_delete: bool,
}

/// Names the input template refers to with `<physical_name>`; the caller
/// needs their staged-file records before [`process_input_template`].
pub fn constant_file_names(template: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut xp = XmlParser::new(template);
    while xp.get_tag() {
        if !xp.is_tag() {
            continue;
        }
        if let Some(n) = xp.parse_str("physical_name") {
            names.push(n);
        }
    }
    names
}

struct FileOut {
    name: String,
}

fn process_file_info(
    xp: &mut XmlParser,
    download_urls: &FileUrls,
    var_infiles: &[InputFile],
    staged: &HashMap<String, StagedFile>,
    out: &mut String,
    infiles: &mut Vec<FileOut>,
    n_var_infiles: &mut usize,
) -> CoreResult<()> {
    let mut gzip = false;
    let mut physical_name = String::new();
    let mut urls: Vec<String> = Vec::new();
    let mut md5str = String::new();
    let mut nbytesdef = -1.0;
    let mut gzipped_nbytes = 0.0;
    out.push_str("<file_info>\n");
    while xp.get_tag() {
        if !xp.is_tag() {
            continue;
        }
        if let Some(v) = xp.parse_bool("gzip") {
            gzip = v;
            continue;
        }
        if let Some(v) = xp.parse_str("physical_name") {
            physical_name = v;
            continue;
        }
        if let Some(v) = xp.parse_str("url") {
            urls.push(v);
            continue;
        }
        if xp.parse_int("number").is_some() {
            continue;
        }
        if let Some(v) = xp.parse_str("md5_cksum") {
            md5str = v;
            continue;
        }
        if let Some(v) = xp.parse_double("nbytes") {
            nbytesdef = v;
            continue;
        }
        if let Some(v) = xp.parse_double("gzipped_nbytes") {
            gzipped_nbytes = v;
            continue;
        }
        if xp.match_tag("/file_info") {
            let var_infile = if physical_name.is_empty() {
                Some(var_infiles.get(*n_var_infiles).ok_or_else(|| {
                    CoreError::Invalid(format!(
                        "too few input files given; need at least {}",
                        *n_var_infiles + 1
                    ))
                })?)
            } else {
                None
            };
            let lookup = |name: &str| {
                staged
                    .get(name)
                    .ok_or_else(|| CoreError::NotFound(format!("input file {name} is not staged")))
            };
            let name;
            if let Some(vf) = var_infile {
                if nbytesdef > 0.0 {
                    // Remote file described in the template.
                    if md5str.is_empty() || urls.is_empty() {
                        return Err(CoreError::Invalid(
                            "input template: url, md5_cksum and nbytes must be given together"
                                .into(),
                        ));
                    }
                    if gzip && gzipped_nbytes == 0.0 {
                        return Err(CoreError::Invalid(
                            "input template: gzipped_nbytes is missing".into(),
                        ));
                    }
                    out.push_str(&format!("    <name>{}</name>\n", vf.name));
                    for u in &urls {
                        out.push_str(&format!("    <url>{}{}</url>\n", u, vf.name));
                        if gzip {
                            out.push_str(&format!(
                                "    <gzipped_url>{}{}.gz</gzipped_url>\n",
                                u, vf.name
                            ));
                        }
                    }
                    out.push_str(&format!(
                        "    <md5_cksum>{md5str}</md5_cksum>\n    <nbytes>{nbytesdef:.0}</nbytes>\n"
                    ));
                    if gzip {
                        out.push_str(&format!(
                            "    <gzipped_nbytes>{gzipped_nbytes:.0}</gzipped_nbytes>\n"
                        ));
                    }
                    name = vf.name.clone();
                } else if let Some(url) = &vf.url {
                    // Remote file given with the job.
                    if vf.md5.is_empty() {
                        return Err(CoreError::Invalid(format!(
                            "remote input file {} needs an md5",
                            vf.name
                        )));
                    }
                    name = format!("jf_{}", vf.md5);
                    out.push_str(&format!(
                        "    <name>{}</name>\n    <url>{}</url>\n    <md5_cksum>{}</md5_cksum>\n    <nbytes>{:.0}</nbytes>\n",
                        name, url, vf.md5, vf.nbytes
                    ));
                } else {
                    if gzip {
                        return Err(CoreError::Invalid(
                            "input template: <gzip/> is not supported for staged files".into(),
                        ));
                    }
                    let st = lookup(&vf.name)?;
                    out.push_str(&format!(
                        "    <name>{}</name>\n    <url>{}</url>\n    <md5_cksum>{}</md5_cksum>\n    <nbytes>{:.0}</nbytes>\n",
                        vf.name,
                        download_urls.download(&vf.name),
                        st.md5,
                        st.nbytes
                    ));
                    if st.no_delete {
                        out.push_str("    <no_delete/>\n");
                    }
                    name = vf.name.clone();
                }
                *n_var_infiles += 1;
            } else {
                let st = lookup(&physical_name)?;
                out.push_str(&format!(
                    "    <name>{}</name>\n    <url>{}</url>\n    <md5_cksum>{}</md5_cksum>\n    <nbytes>{:.0}</nbytes>\n",
                    physical_name,
                    download_urls.download(&physical_name),
                    st.md5,
                    st.nbytes
                ));
                if st.no_delete {
                    out.push_str("    <no_delete/>\n");
                }
                name = physical_name.clone();
            }
            infiles.push(FileOut { name });
            out.push_str("</file_info>\n");
            return Ok(());
        }
        let el = xp.copy_element().map_err(|e| xml_err(e.to_string()))?;
        out.push_str(&el);
        out.push('\n');
    }
    Err(xml_err("input template: <file_info> is not closed"))
}

fn process_workunit(
    xp: &mut XmlParser,
    wu: &mut Workunit,
    out: &mut String,
    infiles: &[FileOut],
    command_line: Option<&str>,
    additional_xml: Option<&str>,
) -> CoreResult<()> {
    let mut n_file_refs = 0;
    out.push_str("<workunit>\n");
    if let Some(c) = command_line {
        out.push_str("<command_line>\n");
        out.push_str(c);
        out.push_str("\n</command_line>\n");
    }
    while xp.get_tag() {
        if !xp.is_tag() {
            continue;
        }
        if xp.match_tag("/workunit") {
            if let Some(x) = additional_xml.filter(|x| !x.is_empty()) {
                out.push_str(x);
                out.push('\n');
            }
            out.push_str("</workunit>");
            return Ok(());
        }
        if xp.match_tag("file_ref") {
            out.push_str("<file_ref>\n");
            let id = infiles.get(n_file_refs).ok_or_else(|| {
                CoreError::Invalid("input template: more <file_ref>s than files".into())
            })?;
            out.push_str(&format!("    <file_name>{}</file_name>\n", id.name));
            let mut found_open_name = false;
            let mut found_copy_file = false;
            let mut closed = false;
            while xp.get_tag() {
                if !xp.is_tag() {
                    continue;
                }
                if let Some(v) = xp.parse_str("open_name") {
                    out.push_str(&format!("    <open_name>{v}</open_name>\n"));
                    found_open_name = true;
                    continue;
                }
                if xp.parse_int("file_number").is_some() {
                    continue;
                }
                if xp.match_tag("copy_file/") {
                    out.push_str("    <copy_file/>\n");
                    found_copy_file = true;
                    continue;
                }
                if xp.match_tag("/file_ref") {
                    if !found_open_name && !found_copy_file {
                        return Err(CoreError::Invalid(
                            "input template: <file_ref> has neither <open_name> nor <copy_file/>"
                                .into(),
                        ));
                    }
                    if !found_open_name {
                        out.push_str(&format!("    <open_name>{}</open_name>\n", id.name));
                    }
                    out.push_str("</file_ref>\n");
                    closed = true;
                    break;
                }
                if xp.parse_str("file_name").is_some() {
                    continue;
                }
                let el = xp.copy_element().map_err(|e| xml_err(e.to_string()))?;
                out.push_str(&el);
                out.push('\n');
            }
            if !closed {
                return Err(xml_err("input template: <file_ref> is not closed"));
            }
            n_file_refs += 1;
            continue;
        }
        if let Some(v) = xp.parse_str("command_line") {
            if command_line.is_some_and(|c| !c.is_empty()) {
                return Err(CoreError::Invalid(
                    "the command line is given both in the input template and with the job".into(),
                ));
            }
            out.push_str("<command_line>\n");
            out.push_str(&v);
            out.push_str("\n</command_line>\n");
            continue;
        }
        macro_rules! num {
            ($method:ident, $name:literal, $dst:expr) => {
                if let Some(v) = xp.$method($name) {
                    $dst = v.into();
                    continue;
                }
            };
        }
        num!(parse_double, "rsc_fpops_est", wu.rsc_fpops_est);
        num!(parse_double, "rsc_fpops_bound", wu.rsc_fpops_bound);
        num!(parse_double, "rsc_memory_bound", wu.rsc_memory_bound);
        num!(parse_double, "rsc_bandwidth_bound", wu.rsc_bandwidth_bound);
        num!(parse_double, "rsc_disk_bound", wu.rsc_disk_bound);
        num!(parse_int, "batch", wu.batch);
        num!(parse_int, "delay_bound", wu.delay_bound);
        num!(parse_int, "min_quorum", wu.min_quorum);
        num!(parse_int, "target_nresults", wu.target_nresults);
        num!(parse_int, "max_error_results", wu.max_error_results);
        num!(parse_int, "max_total_results", wu.max_total_results);
        num!(parse_int, "max_success_results", wu.max_success_results);
        if xp.parse_int("size_class").is_some() {
            // Size classes are not implemented; every job is class-less.
            continue;
        }
        let el = xp.copy_element().map_err(|e| xml_err(e.to_string()))?;
        out.push_str(&el);
        out.push('\n');
    }
    Err(xml_err("input template: <workunit> is not closed"))
}

/// Fills in a workunit from an input template (`process_input_template`):
/// sets `wu.xml_doc` and copies the job parameters the template gives.
///
/// `staged` must hold every non-remote file named in `var_infiles` and every
/// `<physical_name>` of the template.
pub fn process_input_template(
    wu: &mut Workunit,
    template: &str,
    var_infiles: &[InputFile],
    staged: &HashMap<String, StagedFile>,
    download_urls: &FileUrls,
    command_line: Option<&str>,
    additional_xml: Option<&str>,
) -> CoreResult<()> {
    let mut out = String::new();
    let mut infiles: Vec<FileOut> = Vec::new();
    let mut n_var_infiles = 0;
    let mut found_workunit = false;
    let mut xp = XmlParser::new(template);
    while xp.get_tag() {
        if !xp.is_tag() {
            continue;
        }
        if xp.match_tag("input_template") || xp.match_tag("/input_template") {
            continue;
        }
        if xp.match_tag("file_info") {
            process_file_info(
                &mut xp,
                download_urls,
                var_infiles,
                staged,
                &mut out,
                &mut infiles,
                &mut n_var_infiles,
            )?;
            continue;
        }
        if xp.match_tag("workunit") {
            found_workunit = true;
            process_workunit(
                &mut xp,
                wu,
                &mut out,
                &infiles,
                command_line,
                additional_xml,
            )?;
            continue;
        }
        xp.skip_unexpected();
    }
    if !found_workunit {
        return Err(xml_err("input template: no <workunit> element"));
    }
    if n_var_infiles != var_infiles.len() {
        return Err(CoreError::Invalid(format!(
            "{} input files given, but the input template uses {}",
            var_infiles.len(),
            n_var_infiles
        )));
    }
    wu.xml_doc = out;
    Ok(())
}

/// A `<file_info>` of a workunit or result document.
#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub name: String,
    pub no_delete: bool,
    pub max_nbytes: f64,
}

/// The `<file_info>` elements of a workunit or result document.
pub fn file_infos(xml: &str) -> Vec<FileInfo> {
    let mut v = Vec::new();
    let mut xp = XmlParser::new(xml);
    while xp.get_tag() {
        if !xp.is_tag() || !xp.match_tag("file_info") {
            continue;
        }
        let mut fi = FileInfo {
            name: String::new(),
            no_delete: false,
            max_nbytes: 0.0,
        };
        while xp.get_tag() {
            if !xp.is_tag() {
                continue;
            }
            if xp.match_tag("/file_info") {
                break;
            }
            if let Some(n) = xp.parse_str("name") {
                fi.name = n;
                continue;
            }
            if let Some(b) = xp.parse_bool("no_delete") {
                fi.no_delete = b;
                continue;
            }
            if let Some(b) = xp.parse_double("max_nbytes") {
                fi.max_nbytes = b;
                continue;
            }
            xp.skip_unexpected();
        }
        if !fi.name.is_empty() {
            v.push(fi);
        }
    }
    v
}

/// An output file a result's `<file_ref>` names (`OUTPUT_FILE_INFO`).
#[derive(Debug, Clone, PartialEq)]
pub struct OutputFile {
    pub name: String,
    /// The `<open_name>` of the `<file_ref>`: the file's logical name, as
    /// the application knows it. Empty when the template gives none.
    pub open_name: String,
    /// Position of the `<file_ref>` among the result's file references, from
    /// 0, in document order. It counts every reference, so it does not
    /// depend on which files were uploaded.
    pub index: usize,
    pub optional: bool,
    pub no_validate: bool,
    /// The client is told to upload it: its `<file_info>` has an upload
    /// URL (`FILE_INFO::uploadable` in `client/client_types.cpp`). A file
    /// without one stays on the client, which reports the result without
    /// it (`RESULT::is_upload_done`), so no object is ever stored for it.
    pub uploadable: bool,
}

/// The names of the `<file_info>` elements that carry an upload URL: a
/// `<url>` that contains `file_upload_handler`, or an `<upload_url>`
/// (`FILE_INFO::parse`).
fn uploadable_names(xml: &str) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let mut xp = XmlParser::new(xml);
    while xp.get_tag() {
        if !xp.is_tag() || !xp.match_tag("file_info") {
            continue;
        }
        let mut name = String::new();
        let mut upload = false;
        while xp.get_tag() {
            if !xp.is_tag() {
                continue;
            }
            if xp.match_tag("/file_info") {
                break;
            }
            if let Some(n) = xp.parse_str("name") {
                name = n;
                continue;
            }
            if let Some(u) = xp.parse_str("url") {
                upload |= u.contains("file_upload_handler");
                continue;
            }
            if xp.parse_str("upload_url").is_some() {
                upload = true;
                continue;
            }
            xp.skip_unexpected();
        }
        if upload && !name.is_empty() {
            names.insert(name);
        }
    }
    names
}

/// The output files of a result, from its `xml_doc_in`
/// (`get_output_file_infos`).
pub fn output_files(xml_doc_in: &str) -> Vec<OutputFile> {
    let mut v = Vec::new();
    let uploadable = uploadable_names(xml_doc_in);
    let mut xp = XmlParser::new(xml_doc_in);
    while xp.get_tag() {
        if !xp.is_tag() || !xp.match_tag("file_ref") {
            continue;
        }
        let mut f = OutputFile {
            name: String::new(),
            open_name: String::new(),
            index: 0,
            optional: false,
            no_validate: false,
            uploadable: false,
        };
        while xp.get_tag() {
            if !xp.is_tag() {
                continue;
            }
            if xp.match_tag("/file_ref") {
                break;
            }
            if let Some(n) = xp.parse_str("file_name") {
                f.name = n;
                continue;
            }
            if let Some(n) = xp.parse_str("open_name") {
                f.open_name = n;
                continue;
            }
            if let Some(b) = xp.parse_bool("optional") {
                f.optional = b;
                continue;
            }
            if let Some(b) = xp.parse_bool("no_validate") {
                f.no_validate = b;
                continue;
            }
            xp.skip_unexpected();
        }
        if !f.name.is_empty() {
            f.uploadable = uploadable.contains(&f.name);
            f.index = v.len();
            v.push(f);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(base: &str) -> FileUrls {
        FileUrls {
            base: base.into(),
            fanout: 0,
        }
    }

    const OUT_TEMPLATE: &str = "<output_template>\n<file_info>\n    <name><OUTFILE_0/></name>\n    <generated_locally/>\n    <upload_when_present/>\n    <max_nbytes>5000000</max_nbytes>\n    <url><UPLOAD_URL/></url>\n</file_info>\n<result>\n    <file_ref>\n        <file_name><OUTFILE_0/></file_name>\n        <open_name>out</open_name>\n        <optional/>\n    </file_ref>\n</result>\n</output_template>\n";

    #[test]
    fn result_template_without_signer() {
        let doc = process_result_template(
            OUT_TEMPLATE,
            None,
            "wu1_0_r123_",
            "https://x.example/upload/file_upload_handler",
        )
        .unwrap();
        assert!(doc.starts_with("<file_info>\n    <name>wu1_0_r123_0</name>"));
        assert!(doc.contains("<url>https://x.example/upload/file_upload_handler</url>"));
        assert!(doc.ends_with("</result>"));
        assert!(!doc.contains("xml_signature"));
        let outs = output_files(&doc);
        assert_eq!(
            outs,
            vec![OutputFile {
                name: "wu1_0_r123_0".into(),
                open_name: "out".into(),
                index: 0,
                optional: true,
                no_validate: false,
                uploadable: true
            }]
        );
        let fis = file_infos(&doc);
        assert_eq!(fis.len(), 1);
        assert_eq!(fis[0].max_nbytes, 5000000.0);
    }

    #[test]
    fn output_files_carry_open_name_and_position() {
        let doc = "<result>\
<file_ref><file_name>a</file_name><open_name>first</open_name></file_ref>\
<file_ref><file_name>b</file_name><optional/></file_ref>\
<file_ref><open_name>nameless</open_name></file_ref>\
<file_ref><open_name>c</open_name><file_name>c</file_name></file_ref>\
</result>";
        let got: Vec<(String, String, usize)> = output_files(doc)
            .into_iter()
            .map(|f| (f.name, f.open_name, f.index))
            .collect();
        assert_eq!(
            got,
            [
                ("a".to_string(), "first".to_string(), 0),
                ("b".to_string(), String::new(), 1),
                ("c".to_string(), "c".to_string(), 2),
            ]
        );
    }

    #[test]
    fn only_files_with_an_upload_url_are_uploadable() {
        let doc = "<file_info><name>a</name><url>https://x.example/upload/file_upload_handler</url></file_info>\
<file_info><name>b</name><generated_locally/></file_info>\
<file_info><name>c</name><url>https://x.example/download/c</url></file_info>\
<file_info><name>d</name><upload_url>https://x.example/u</upload_url></file_info>\
<result>\
<file_ref><file_name>a</file_name></file_ref>\
<file_ref><file_name>b</file_name></file_ref>\
<file_ref><file_name>c</file_name></file_ref>\
<file_ref><file_name>d</file_name><optional/></file_ref>\
<file_ref><file_name>e</file_name></file_ref>\
</result>";
        let up: Vec<(String, bool)> = output_files(doc)
            .into_iter()
            .map(|f| (f.name, f.uploadable))
            .collect();
        assert_eq!(
            up,
            [
                ("a".to_string(), true),
                ("b".to_string(), false),
                ("c".to_string(), false),
                ("d".to_string(), true),
                ("e".to_string(), false),
            ]
        );
    }

    #[test]
    fn result_template_signs_each_file() {
        let (private, public) = cb_crypto::generate_key_pair().unwrap();
        let signer = Signer::new(&private).unwrap();
        let doc = process_result_template(OUT_TEMPLATE, Some(&signer), "wu1_0_r123_", "u").unwrap();
        let start = doc.find("<xml_signature>\n").unwrap() + "<xml_signature>\n".len();
        let end = doc.find("</xml_signature>").unwrap();
        assert!(cb_crypto::verify_upload_certificate(
            "wu1_0_r123_0",
            5000000.0,
            &doc[start..end],
            &public
        ));
        assert!(doc[end..].starts_with("</xml_signature>\n</file_info>"));
    }

    #[test]
    fn malformed_outfile_macro_is_rejected() {
        assert!(process_result_template("<name><OUTFILE_0></name>", None, "b", "u").is_err());
    }

    #[test]
    fn input_template_with_staged_and_remote_files() {
        let template = "<input_template>\n<file_info>\n    <number>0</number>\n</file_info>\n<file_info>\n    <number>1</number>\n    <sticky/>\n</file_info>\n<workunit>\n    <file_ref>\n        <file_number>0</file_number>\n        <open_name>in</open_name>\n    </file_ref>\n    <file_ref>\n        <file_number>1</file_number>\n        <copy_file/>\n    </file_ref>\n    <rsc_fpops_est>1e12</rsc_fpops_est>\n    <delay_bound>3600</delay_bound>\n    <min_quorum>2</min_quorum>\n</workunit>\n</input_template>\n";
        let mut staged = HashMap::new();
        staged.insert(
            "a.txt".to_string(),
            StagedFile {
                md5: "0cc175b9c0f1b6a831c399e269772661".into(),
                nbytes: 1.0,
                no_delete: false,
            },
        );
        let files = vec![
            InputFile {
                name: "a.txt".into(),
                ..Default::default()
            },
            InputFile {
                name: "b".into(),
                url: Some("https://other.example/b".into()),
                md5: "92eb5ffee6ae2fec3ad71c777531578f".into(),
                nbytes: 7.0,
            },
        ];
        let mut wu = Workunit::default();
        process_input_template(
            &mut wu,
            template,
            &files,
            &staged,
            &flat("https://x.example/download"),
            Some("--n 3"),
            None,
        )
        .unwrap();
        assert_eq!(wu.rsc_fpops_est, 1e12);
        assert_eq!(wu.delay_bound, 3600);
        assert_eq!(wu.min_quorum, 2);
        let expect = "<file_info>\n    <name>a.txt</name>\n    <url>https://x.example/download/a.txt</url>\n    <md5_cksum>0cc175b9c0f1b6a831c399e269772661</md5_cksum>\n    <nbytes>1</nbytes>\n</file_info>\n<file_info>\n<sticky/>\n    <name>jf_92eb5ffee6ae2fec3ad71c777531578f</name>\n    <url>https://other.example/b</url>\n    <md5_cksum>92eb5ffee6ae2fec3ad71c777531578f</md5_cksum>\n    <nbytes>7</nbytes>\n</file_info>\n<workunit>\n<command_line>\n--n 3\n</command_line>\n<file_ref>\n    <file_name>a.txt</file_name>\n    <open_name>in</open_name>\n</file_ref>\n<file_ref>\n    <file_name>jf_92eb5ffee6ae2fec3ad71c777531578f</file_name>\n    <copy_file/>\n    <open_name>jf_92eb5ffee6ae2fec3ad71c777531578f</open_name>\n</file_ref>\n</workunit>";
        assert_eq!(wu.xml_doc, expect);
    }

    #[test]
    fn kept_staged_file_is_marked_no_delete() {
        let template = "<file_info>\n<number>0</number>\n</file_info>\n<workunit>\n<file_ref><file_number>0</file_number><open_name>in</open_name></file_ref>\n</workunit>";
        let mut staged = HashMap::new();
        staged.insert(
            "shared.txt".to_string(),
            StagedFile {
                md5: "0cc175b9c0f1b6a831c399e269772661".into(),
                nbytes: 1.0,
                no_delete: true,
            },
        );
        let files = vec![InputFile {
            name: "shared.txt".into(),
            ..Default::default()
        }];
        let mut wu = Workunit::default();
        process_input_template(
            &mut wu,
            template,
            &files,
            &staged,
            &flat("https://x.example/download"),
            None,
            None,
        )
        .unwrap();
        let kept: Vec<_> = file_infos(&wu.xml_doc)
            .into_iter()
            .map(|f| (f.name, f.no_delete))
            .collect();
        assert_eq!(kept, [("shared.txt".to_string(), true)]);
    }

    #[test]
    fn input_template_needs_every_file() {
        let template = "<file_info>\n<number>0</number>\n</file_info>\n<workunit>\n<file_ref><file_number>0</file_number><open_name>in</open_name></file_ref>\n</workunit>";
        let mut wu = Workunit::default();
        let err = process_input_template(
            &mut wu,
            template,
            &[],
            &HashMap::new(),
            &flat("d"),
            None,
            None,
        );
        assert!(matches!(err, Err(CoreError::Invalid(_))));
    }
}
