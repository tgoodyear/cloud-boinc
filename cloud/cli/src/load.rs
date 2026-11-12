//! `cboinc load`: simulated hosts that fetch work, upload an output file
//! per result and report, the way a client does, with latencies measured.
//!
//! With `--cut-at`, every upload is first interrupted after that many bytes
//! of file data, then resumed the way a client resumes it: a
//! `<get_file_size>` query, and the rest of the file sent with `<offset>`
//! set to the answer.

use std::cell::Cell;
use std::io::{self, Read};
use std::thread;
use std::time::{Duration, Instant};

use cb_crypto::md5_hex;
use clap::Args;
use serde_json::json;

#[derive(Args, Clone)]
pub struct LoadArgs {
    /// Base URL of the deployment under test.
    #[arg(long, env = "CBOINC_URL")]
    url: String,
    /// Authenticator of the account the hosts attach to.
    #[arg(long)]
    authenticator: String,
    #[arg(long, default_value = "x86_64-pc-linux-gnu")]
    platform: String,
    /// Simulated hosts, one thread each.
    #[arg(long, default_value_t = 4)]
    hosts: u32,
    /// Scheduler requests per host. Each reports the results of the one
    /// before and asks for more work.
    #[arg(long, default_value_t = 5)]
    rounds: u32,
    /// Seconds of work to ask for; decides how many jobs a request gets.
    #[arg(long, default_value_t = 1.0)]
    work_seconds: f64,
    /// Size of the output file uploaded for each result.
    #[arg(long, default_value_t = 1024)]
    output_bytes: usize,
    /// Pause between a host's requests.
    #[arg(long, default_value_t = 0)]
    think_ms: u64,
    /// Distinguishes the hosts of this run from those of another.
    #[arg(long, default_value = "load")]
    run_name: String,
    /// Interrupt each upload after this many bytes of file data, then
    /// resume it. Outputs no longer than this are sent whole.
    #[arg(long)]
    cut_at: Option<usize>,
    /// How the first attempt is interrupted: `abort` drops the connection
    /// in the middle of the request body; `short` sends a complete request
    /// whose data stops at the cut.
    #[arg(long, value_enum, default_value_t = CutMode::Abort)]
    cut_mode: CutMode,
    /// With `--cut-mode abort`, how long to wait after sending the last
    /// byte before dropping the connection.
    #[arg(long, default_value_t = 0)]
    cut_pause_ms: u64,
    /// How long to wait after the interrupted attempt before asking for
    /// the size and resuming. A real client waits at least a minute.
    #[arg(long, default_value_t = 0)]
    resume_wait_ms: u64,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum CutMode {
    Abort,
    Short,
}

/// A request body that fails after its `data`, and after a pause, so the
/// HTTP client drops the connection part-way through the upload.
struct CutBody {
    data: Vec<u8>,
    pos: usize,
    pause: Duration,
}

impl Read for CutBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.data.len() {
            thread::sleep(self.pause);
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "upload cut on purpose",
            ));
        }
        let n = buf.len().min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[derive(Default)]
struct Tally {
    sched_ms: Vec<f64>,
    upload_ms: Vec<f64>,
    /// Every HTTP request a host sent, successful or not: scheduler
    /// requests, uploads, the interrupted first attempts and size queries.
    /// The timings above are only for the requests they name.
    requests: u64,
    /// Bytes the server reported holding after each cut upload.
    resumed_from: Vec<u64>,
    results_received: u64,
    results_reported: u64,
    errors: Vec<String>,
}

fn element<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(xml[start..end].trim())
}

/// The contents of each `<tag>…</tag>` at any depth, in order.
fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let body = &rest[i + open.len()..];
        let Some(j) = body.find(&close) else { break };
        out.push(&body[..j]);
        rest = &body[j + close.len()..];
    }
    out
}

struct Task {
    name: String,
    /// `(file name, <file_info> contents as the server sent them)`.
    outputs: Vec<(String, String)>,
}

struct Host {
    args: LoadArgs,
    index: u32,
    agent: ureq::Agent,
    hostid: i64,
    seqno: u32,
    version_num: i32,
    /// Requests sent so far, whether or not they succeeded.
    requests: Cell<u64>,
    /// Results to report with the next request: `(name, <result> element)`.
    reports: Vec<(String, String)>,
}

impl Host {
    fn request_xml(&self, work: bool) -> String {
        let w = if work { self.args.work_seconds } else { 0.0 };
        let cpid = md5_hex(format!("{}-{}", self.args.run_name, self.index));
        let mut s = format!(
            "<scheduler_request>\n    <authenticator>{}</authenticator>\n    <hostid>{}</hostid>\n    <rpc_seqno>{}</rpc_seqno>\n    <core_client_major_version>8</core_client_major_version>\n    <core_client_minor_version>0</core_client_minor_version>\n    <core_client_release>4</core_client_release>\n<client_cap_plan_class>1</client_cap_plan_class>\n    <work_req_seconds>{w}</work_req_seconds>\n    <cpu_req_secs>{w}</cpu_req_secs>\n    <cpu_req_instances>{}</cpu_req_instances>\n    <estimated_delay>0</estimated_delay>\n    <platform_name>{}</platform_name>\n",
            self.args.authenticator,
            self.hostid,
            self.seqno,
            if work { 1 } else { 0 },
            self.args.platform,
        );
        s.push_str(&format!(
            "<host_info>\n    <domain_name>{}-{}</domain_name>\n    <host_cpid>{cpid}</host_cpid>\n    <p_ncpus>4</p_ncpus>\n    <p_vendor>GenuineIntel</p_vendor>\n    <p_model>load generator</p_model>\n    <p_fpops>2e9</p_fpops>\n    <p_iops>1e10</p_iops>\n    <m_nbytes>8e9</m_nbytes>\n    <d_total>1e11</d_total>\n    <d_free>5e10</d_free>\n    <os_name>Linux</os_name>\n    <os_version>6.1</os_version>\n</host_info>\n",
            self.args.run_name, self.index
        ));
        for (_, r) in &self.reports {
            s.push_str(r);
        }
        s.push_str("</scheduler_request>\n");
        s
    }

    fn sched(&mut self, work: bool, tally: &mut Tally) -> Result<Vec<Task>, String> {
        self.seqno += 1;
        let body = self.request_xml(work);
        self.requests.set(self.requests.get() + 1);
        let started = Instant::now();
        let reply = self
            .agent
            .post(&format!("{}/sched", self.args.url.trim_end_matches('/')))
            .set("Content-Type", "text/xml")
            .send_string(&body)
            .map_err(|e| format!("sched: {e}"))?
            .into_string()
            .map_err(|e| format!("sched: {e}"))?;
        tally.sched_ms.push(started.elapsed().as_secs_f64() * 1e3);
        if !reply.contains("<scheduler_reply>") {
            return Err(format!(
                "sched: not a scheduler reply: {}",
                reply.chars().take(200).collect::<String>()
            ));
        }
        if let Some(id) = element(&reply, "hostid").and_then(|v| v.parse().ok()) {
            self.hostid = id;
        }
        if self.hostid == 0 {
            return Err(format!(
                "sched: no host ID: {}",
                blocks(&reply, "message").join("; ")
            ));
        }
        if reply.contains("<project_is_down/>") {
            return Err("sched: the project is down".into());
        }
        // A report stays queued until the server acknowledges it by name.
        let acked: Vec<&str> = blocks(&reply, "result_ack")
            .into_iter()
            .filter_map(|b| element(b, "name"))
            .collect();
        let before = self.reports.len();
        self.reports
            .retain(|(name, _)| !acked.contains(&name.as_str()));
        tally.results_reported += (before - self.reports.len()) as u64;
        if let Some(v) = blocks(&reply, "app_version")
            .first()
            .and_then(|b| element(b, "version_num"))
            .and_then(|v| v.parse().ok())
        {
            self.version_num = v;
        }
        let infos: Vec<(String, &str)> = blocks(&reply, "file_info")
            .into_iter()
            .filter(|b| b.contains("<generated_locally/>"))
            .filter_map(|b| Some((element(b, "name")?.to_string(), b)))
            .collect();
        let mut tasks = Vec::new();
        for r in blocks(&reply, "result") {
            let Some(name) = element(r, "name") else {
                continue;
            };
            let outputs = blocks(r, "file_ref")
                .into_iter()
                .filter_map(|f| element(f, "file_name"))
                .filter_map(|n| {
                    infos
                        .iter()
                        .find(|(name, _)| name == n)
                        .map(|(name, b)| (name.clone(), b.to_string()))
                })
                .collect();
            tasks.push(Task {
                name: name.to_string(),
                outputs,
            });
        }
        tally.results_received += tasks.len() as u64;
        Ok(tasks)
    }

    fn upload_header(info: &str, data: &[u8], offset: usize) -> Vec<u8> {
        format!(
            "<data_server_request>\n    <core_client_major_version>8</core_client_major_version>\n    <core_client_minor_version>0</core_client_minor_version>\n    <core_client_release>4</core_client_release>\n<file_upload>\n<file_info>{info}</file_info>\n<nbytes>{}</nbytes>\n<md5_cksum>{}</md5_cksum>\n<offset>{offset}</offset>\n<data>\n",
            data.len(),
            md5_hex(data)
        )
        .into_bytes()
    }

    fn post(&self, url: &str, body: &[u8]) -> Result<String, String> {
        self.requests.set(self.requests.get() + 1);
        self.agent
            .post(url)
            .send_bytes(body)
            .map_err(|e| format!("upload: {e}"))?
            .into_string()
            .map_err(|e| format!("upload: {e}"))
    }

    /// The first, interrupted attempt and the size query after it. Returns
    /// the offset to resume from.
    fn cut_upload(&self, url: &str, info: &str, data: &[u8], cut: usize) -> Result<usize, String> {
        let mut body = Self::upload_header(info, data, 0);
        body.extend_from_slice(&data[..cut]);
        match self.args.cut_mode {
            CutMode::Short => {
                let reply = self.post(url, &body)?;
                if element(&reply, "status") == Some("0") {
                    return Err(format!(
                        "upload: a cut upload was answered with success: {}",
                        reply.replace('\n', " ")
                    ));
                }
            }
            // The request fails on the client side; what matters is what the
            // server kept. Like a real client (`HTTP_OP::libcurl_exec` in
            // `client/http_curl.cpp`), it announces the length of the whole
            // request.
            CutMode::Abort => {
                let full = body.len() - cut + data.len();
                self.requests.set(self.requests.get() + 1);
                let _ = self
                    .agent
                    .post(url)
                    .set("Content-Length", &full.to_string())
                    .send(CutBody {
                        data: body,
                        pos: 0,
                        pause: Duration::from_millis(self.args.cut_pause_ms),
                    });
            }
        }
        thread::sleep(Duration::from_millis(self.args.resume_wait_ms));
        let name = element(info, "name").ok_or("upload: the file has no name")?;
        let query = format!(
            "<data_server_request>\n    <core_client_major_version>8</core_client_major_version>\n    <core_client_minor_version>0</core_client_minor_version>\n    <core_client_release>4</core_client_release>\n    <get_file_size>{name}</get_file_size>\n</data_server_request>\n"
        );
        let reply = self.post(url, query.as_bytes())?;
        let held: usize = element(&reply, "file_size")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("get_file_size: {}", reply.replace('\n', " ")))?;
        if held > cut {
            return Err(format!(
                "get_file_size: the server reports {held} bytes of {name}, but only {cut} were sent"
            ));
        }
        Ok(held)
    }

    fn upload(&self, info: &str, data: &[u8], tally: &mut Tally) -> Result<(), String> {
        let url = element(info, "url").ok_or("upload: the file has no URL")?;
        let offset = match self.args.cut_at {
            Some(cut) if cut < data.len() => {
                let held = self.cut_upload(url, info, data, cut)?;
                tally.resumed_from.push(held as u64);
                held
            }
            _ => 0,
        };
        let mut body = Self::upload_header(info, data, offset);
        body.extend_from_slice(&data[offset..]);
        let started = Instant::now();
        let reply = self.post(url, &body)?;
        tally.upload_ms.push(started.elapsed().as_secs_f64() * 1e3);
        if element(&reply, "status") != Some("0") {
            return Err(format!("upload: {}", reply.replace('\n', " ")));
        }
        Ok(())
    }

    /// Uploads the outputs of a task and queues its report.
    fn complete(&mut self, task: &Task, tally: &mut Tally) -> Result<(), String> {
        let mut files = String::new();
        for (name, info) in &task.outputs {
            let seed = md5_hex(name);
            let data: Vec<u8> = seed.bytes().cycle().take(self.args.output_bytes).collect();
            self.upload(info, &data, tally)?;
            files.push_str(&format!(
                "<file_info>\n    <name>{name}</name>\n    <nbytes>{}</nbytes>\n    <max_nbytes>{}</max_nbytes>\n    <md5_cksum>{}</md5_cksum>\n</file_info>\n",
                data.len(),
                element(info, "max_nbytes").unwrap_or("0"),
                md5_hex(&data)
            ));
        }
        self.reports.push((
            task.name.clone(),
            format!(
            "<result>\n    <name>{}</name>\n    <final_cpu_time>1</final_cpu_time>\n    <final_elapsed_time>1</final_elapsed_time>\n    <exit_status>0</exit_status>\n    <state>5</state>\n    <app_version_num>{}</app_version_num>\n{files}</result>\n",
            task.name, self.version_num
        )));
        Ok(())
    }

    fn run(mut self) -> Tally {
        let mut tally = Tally::default();
        for round in 0..=self.args.rounds {
            // The last request only reports.
            let work = round < self.args.rounds;
            if !work && self.reports.is_empty() {
                break;
            }
            match self.sched(work, &mut tally) {
                Ok(tasks) => {
                    for t in &tasks {
                        if let Err(e) = self.complete(t, &mut tally) {
                            tally.errors.push(e);
                        }
                    }
                }
                Err(e) => tally.errors.push(e),
            }
            if self.args.think_ms > 0 {
                thread::sleep(Duration::from_millis(self.args.think_ms));
            }
        }
        if !self.reports.is_empty() {
            tally.errors.push(format!(
                "{} reports were not acknowledged",
                self.reports.len()
            ));
        }
        tally.requests = self.requests.get();
        tally
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    (sorted[i] * 10.0).round() / 10.0
}

fn summary(mut ms: Vec<f64>) -> serde_json::Value {
    ms.sort_by(|a, b| a.total_cmp(b));
    json!({
        "count": ms.len(),
        "p50_ms": percentile(&ms, 0.5),
        "p90_ms": percentile(&ms, 0.9),
        "p99_ms": percentile(&ms, 0.99),
        "max_ms": percentile(&ms, 1.0),
    })
}

pub fn run(args: &LoadArgs) -> Result<(), String> {
    let started = Instant::now();
    let handles: Vec<_> = (0..args.hosts)
        .map(|index| {
            let host = Host {
                args: args.clone(),
                index,
                agent: ureq::AgentBuilder::new()
                    .timeout(Duration::from_secs(60))
                    .build(),
                hostid: 0,
                seqno: 0,
                version_num: 0,
                requests: Cell::new(0),
                reports: Vec::new(),
            };
            thread::spawn(move || host.run())
        })
        .collect();
    let mut total = Tally::default();
    for h in handles {
        let t = h.join().map_err(|_| "a host thread panicked".to_string())?;
        total.sched_ms.extend(t.sched_ms);
        total.upload_ms.extend(t.upload_ms);
        total.requests += t.requests;
        total.resumed_from.extend(t.resumed_from);
        total.results_received += t.results_received;
        total.results_reported += t.results_reported;
        total.errors.extend(t.errors);
    }
    let seconds = started.elapsed().as_secs_f64();
    let requests = total.requests;
    let failed = !total.errors.is_empty();
    total.errors.sort();
    total.errors.dedup();
    total.errors.truncate(10);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "hosts": args.hosts,
            "seconds": (seconds * 10.0).round() / 10.0,
            "requests_per_second": (requests as f64 / seconds * 10.0).round() / 10.0,
            "scheduler": summary(total.sched_ms),
            "upload": summary(total.upload_ms),
            "resumed_uploads": total.resumed_from.len(),
            "resumed_from_bytes": {
                "min": total.resumed_from.iter().min(),
                "max": total.resumed_from.iter().max(),
            },
            "results_received": total.results_received,
            "results_reported": total.results_reported,
            "errors": total.errors,
        }))
        .map_err(|e| e.to_string())?
    );
    if failed {
        return Err("some requests failed".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    /// A server on the loopback interface that answers the three requests
    /// of one cut upload: the interrupted attempt (an error), the size
    /// query, and the resumed upload.
    fn serve(listener: TcpListener) {
        thread::spawn(move || {
            let mut uploads = 0;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head_end, length) = loop {
                    let n = stream.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (i + 4, length);
                    }
                };
                while buf.len() < head_end + length {
                    let n = stream.read(&mut chunk).unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = String::from_utf8_lossy(&buf[head_end..]).into_owned();
                let reply = if body.contains("<get_file_size>") {
                    "<data_server_reply><status>0</status><file_size>0</file_size></data_server_reply>"
                } else {
                    uploads += 1;
                    if uploads == 1 {
                        "<data_server_reply><status>1</status></data_server_reply>"
                    } else {
                        "<data_server_reply><status>0</status></data_server_reply>"
                    }
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
            }
        });
    }

    #[test]
    fn a_cut_upload_counts_all_three_requests_and_times_only_the_last() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/upload/file_upload_handler",
            listener.local_addr().unwrap()
        );
        serve(listener);
        let args = LoadArgs {
            url: url.clone(),
            authenticator: "a".into(),
            platform: "p".into(),
            hosts: 1,
            rounds: 1,
            work_seconds: 1.0,
            output_bytes: 100,
            think_ms: 0,
            run_name: "t".into(),
            cut_at: Some(10),
            cut_mode: CutMode::Short,
            cut_pause_ms: 0,
            resume_wait_ms: 0,
        };
        let host = Host {
            args,
            index: 0,
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(10))
                .build(),
            hostid: 0,
            seqno: 0,
            version_num: 0,
            requests: Cell::new(0),
            reports: Vec::new(),
        };
        let info = format!("<name>f</name><url>{url}</url><max_nbytes>1000</max_nbytes>");
        let mut tally = Tally::default();
        host.upload(&info, &[7u8; 100], &mut tally).unwrap();
        // The interrupted attempt, the size query and the resumed upload.
        assert_eq!(host.requests.get(), 3);
        // Only the resumed upload is timed as an upload.
        assert_eq!(tally.upload_ms.len(), 1);
        assert_eq!(tally.resumed_from, [0]);

        // Without a cut, one request.
        let mut host = host;
        host.args.cut_at = None;
        host.requests.set(0);
        let mut tally = Tally::default();
        host.upload(&info, &[7u8; 100], &mut tally).unwrap();
        assert_eq!(host.requests.get(), 1);
    }
}
