//! What the site's computer pages say about a host's GPUs, Docker or Podman,
//! VirtualBox and client version, read from `host.misc` (the JSON the
//! scheduler stores, see `host_misc.rs`), with `host.serialnum` as the source
//! for rows that have no usable `misc`.
//!
//! This follows `html/inc/host.inc` in the stock tree: `gpu_desc`,
//! `vbox_desc`, `docker_desc` and `boinc_version` read `json_decode($host->misc)`.
//! Stock does not fall back; a row with no `misc` shows `---`. Here such a row
//! (a host that has not contacted the server since `misc` was first stored,
//! or a value that is not JSON) is described from `serialnum`, in the old
//! layout `html/ops/hostinfo.php` converts to JSON (`to_struct`:
//! `[BOINC|8.2.4][CUDA|model|count|ram|driver|opencl][vbox|7.1.6|1|1]`) and in
//! the shorter one this server's scheduler wrote before it stored `misc`
//! (`[CUDA|2|8192MB]`, `[docker|27.0.3]`).
//!
//! Every value is escaped; stock escapes some (`htmlspecialchars` on versions)
//! and writes the GPU model unescaped. A value of the wrong type in the JSON
//! reads as PHP would read it (a number as a string, a string as an integer by
//! its leading digits) and never fails.

use serde_json::Value;

use crate::web::html_escape;

const KILO: f64 = 1024.0;
const MEGA: f64 = KILO * KILO;
const GIGA: f64 = MEGA * KILO;
const TERA: f64 = GIGA * KILO;

/// One entry of `gpus`.
#[derive(Debug, Default, Clone, PartialEq)]
struct Gpu {
    model: String,
    count: i64,
    ram_mb: i64,
    driver_version: String,
    opencl_version: i64,
}

/// The distribution Docker or Podman runs in, on Windows.
#[derive(Debug, Clone, PartialEq)]
struct Distro {
    name: String,
    /// `boinc_buda_runner_version`; 0 when absent.
    runner_version: i64,
}

#[derive(Debug, Clone, PartialEq)]
struct Docker {
    version: String,
    /// 1 Docker, anything else Podman, as `docker_desc` has it. `UNKNOWN`
    /// for a `serialnum` that did not record it.
    kind: i64,
    distro: Option<Distro>,
}

const UNKNOWN: i64 = -1;

#[derive(Debug, Clone, PartialEq)]
struct Vbox {
    version: String,
    hw_accel: Option<bool>,
    hw_accel_enabled: Option<bool>,
}

/// The parts of `host.misc` the pages show.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct HostMisc {
    client_version: Option<String>,
    client_brand: String,
    gpus: Vec<Gpu>,
    docker: Option<Docker>,
    vbox: Option<Vbox>,
    dont_use_wsl: bool,
    dont_use_docker: bool,
}

/// PHP `empty()` of a JSON value.
fn empty(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => !b,
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        Some(Value::String(s)) => s.is_empty() || s == "0",
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Object(o)) => o.is_empty(),
    }
}

/// PHP's string conversion of a scalar; structures read as empty.
fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(true)) => "1".into(),
        _ => String::new(),
    }
}

/// PHP `intval` of a string: the leading sign and digits.
fn intval_str(s: &str) -> i64 {
    let s = s.trim_start();
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let n = digits.parse::<i64>().unwrap_or(0);
    if neg {
        -n
    } else {
        n
    }
}

/// PHP `intval` of a JSON value.
fn int(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => intval_str(s),
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

fn flag(v: Option<&Value>) -> Option<bool> {
    match v {
        None | Some(Value::Null) => None,
        Some(v) => Some(!empty(Some(v))),
    }
}

impl HostMisc {
    /// The host's description: `misc` when it is a JSON object with
    /// something in it, otherwise `serialnum`.
    pub fn read(misc: &str, serialnum: &str) -> HostMisc {
        match Self::from_json(misc) {
            Some(m) => m,
            None => Self::from_serialnum(serialnum),
        }
    }

    /// `host.misc`; `None` when it is empty, not JSON, or not an object.
    pub fn from_json(misc: &str) -> Option<HostMisc> {
        if misc.trim().is_empty() {
            return None;
        }
        let Ok(Value::Object(o)) = serde_json::from_str::<Value>(misc) else {
            return None;
        };
        if o.is_empty() {
            return None;
        }
        let mut m = HostMisc {
            client_version: o
                .get("client_version")
                .filter(|v| !v.is_null())
                .map(|v| text(Some(v))),
            client_brand: text(o.get("client_brand")),
            ..Default::default()
        };
        if let Some(Value::Array(list)) = o.get("gpus") {
            for g in list {
                let Value::Object(g) = g else { continue };
                let count = int(g.get("count"));
                m.gpus.push(Gpu {
                    model: text(g.get("model")),
                    count: if count == 0 { 1 } else { count },
                    ram_mb: if empty(g.get("ram_mb")) {
                        0
                    } else {
                        int(g.get("ram_mb"))
                    },
                    driver_version: if empty(g.get("driver_version")) {
                        String::new()
                    } else {
                        text(g.get("driver_version"))
                    },
                    opencl_version: if empty(g.get("opencl_version")) {
                        0
                    } else {
                        int(g.get("opencl_version"))
                    },
                });
            }
        }
        if let Some(Value::Object(d)) = o.get("docker") {
            // `docker_desc` takes the distribution from `wsl_distro` when that
            // is an object (Science United once wrote it that way), else from
            // the docker object itself.
            let distro = match d.get("wsl_distro") {
                None | Some(Value::Null) => None,
                Some(Value::Object(inner)) => Some(Distro {
                    name: text(inner.get("wsl_distro")),
                    runner_version: if empty(inner.get("boinc_buda_runner_version")) {
                        0
                    } else {
                        int(inner.get("boinc_buda_runner_version"))
                    },
                }),
                Some(name) => Some(Distro {
                    name: text(Some(name)),
                    runner_version: if empty(d.get("boinc_buda_runner_version")) {
                        0
                    } else {
                        int(d.get("boinc_buda_runner_version"))
                    },
                }),
            };
            m.docker = Some(Docker {
                version: text(d.get("version")),
                kind: int(d.get("type")),
                distro,
            });
        }
        if let Some(Value::Object(v)) = o.get("vbox") {
            m.vbox = Some(Vbox {
                version: text(v.get("version")),
                hw_accel: flag(v.get("hw_accel")),
                hw_accel_enabled: flag(v.get("hw_accel_enabled")),
            });
        }
        if let Some(Value::Object(c)) = o.get("config") {
            m.dont_use_wsl = !empty(c.get("dont_use_wsl"));
            m.dont_use_docker = !empty(c.get("dont_use_docker"));
        }
        Some(m)
    }

    /// `host.serialnum`, in either layout (see the module comment).
    pub fn from_serialnum(serialnum: &str) -> HostMisc {
        let mut m = HostMisc::default();
        for part in serialnum.split('[').filter_map(|p| p.strip_suffix(']')) {
            let x: Vec<&str> = part.split('|').collect();
            let field = |i: usize| x.get(i).copied().unwrap_or("");
            match x[0] {
                "BOINC" => {
                    m.client_version = Some(field(1).to_string());
                    m.client_brand = field(2).to_string();
                }
                "Microsoft" => {}
                "dont_use_docker" => m.dont_use_docker = true,
                "dont_use_wsl" => m.dont_use_wsl = true,
                "vbox" => {
                    m.vbox = Some(Vbox {
                        version: field(1).to_string(),
                        hw_accel: (x.len() >= 4).then(|| !matches!(field(2), "" | "0")),
                        hw_accel_enabled: (x.len() >= 4).then(|| !matches!(field(3), "" | "0")),
                    })
                }
                "docker" => {
                    m.docker = Some(Docker {
                        version: field(1).to_string(),
                        kind: if x.len() >= 3 {
                            intval_str(field(2))
                        } else {
                            UNKNOWN
                        },
                        distro: None,
                    })
                }
                "opencl_gpu"
                    if {
                        let model = field(1).to_lowercase();
                        model.contains("apple") || model.contains("amd")
                    } => {}
                kind => {
                    if !matches!(
                        kind,
                        "CUDA" | "CAL" | "INTEL" | "ARM" | "apple_gpu" | "Apple" | "opencl_gpu"
                    ) && !(x.len() == 3 && field(2).ends_with("MB"))
                    {
                        continue;
                    }
                    if x.len() == 3 {
                        // This server's own layout: `[kind|count|ram]`.
                        m.gpus.push(Gpu {
                            model: kind.to_string(),
                            count: intval_str(field(1)).max(1),
                            ram_mb: intval_str(field(2)),
                            ..Default::default()
                        });
                    } else {
                        // `make_gpu` in `html/ops/hostinfo.php`.
                        m.gpus.push(Gpu {
                            model: field(1).to_string(),
                            count: match intval_str(field(2)) {
                                0 => 1,
                                n => n,
                            },
                            ram_mb: intval_str(field(3)),
                            driver_version: if field(4) == "0" {
                                String::new()
                            } else {
                                field(4).to_string()
                            },
                            opencl_version: intval_str(field(5)),
                        });
                    }
                }
            }
        }
        m
    }

    /// `boinc_version`: the client version and its brand. Escaped.
    pub fn boinc_version(&self) -> String {
        let Some(v) = &self.client_version else {
            return "---".into();
        };
        let mut x = html_escape(v);
        if !self.client_brand.is_empty() && self.client_brand != "0" {
            x.push_str(&format!(" ({})", html_escape(&self.client_brand)));
        }
        x
    }

    /// `gpu_desc`: one line for each kind of GPU, with the count, model,
    /// memory, driver and OpenCL level. Escaped.
    pub fn gpu_desc(&self) -> String {
        if self.gpus.is_empty() {
            return "---".into();
        }
        let lines: Vec<String> = self
            .gpus
            .iter()
            .map(|g| {
                let mut x = format!("{} {}", g.count, html_escape(&g.model));
                if g.ram_mb != 0 {
                    x.push_str(&format!("; {} RAM", size_string(g.ram_mb as f64 * MEGA)));
                }
                if !g.driver_version.is_empty() {
                    x.push_str(&format!("; Driver: {}", html_escape(&g.driver_version)));
                }
                if g.opencl_version != 0 {
                    // `%d.%d` of `intval/100` and `intval%100`: 300 is 3.0, 102 is 1.2.
                    x.push_str(&format!(
                        "; OpenCL: {}.{}",
                        g.opencl_version / 100,
                        g.opencl_version % 100
                    ));
                }
                x
            })
            .collect();
        lines.join("<br>")
    }

    /// `vbox_desc`. Escaped.
    pub fn vbox_desc(&self) -> String {
        let Some(v) = &self.vbox else {
            return "---".into();
        };
        let yes = |b: bool| if b { "yes" } else { "no" };
        let mut x = format!("Virtualbox {}", html_escape(&v.version));
        if let Some(b) = v.hw_accel {
            x.push_str(&format!("<br>HW acceleration: {}", yes(b)));
        }
        if let Some(b) = v.hw_accel_enabled {
            x.push_str(&format!("<br>Enabled: {}", yes(b)));
        }
        x
    }

    /// `docker_desc`: Docker or Podman with its version, the WSL distribution
    /// it runs in, and the client's own settings. Escaped.
    pub fn docker_desc(&self) -> String {
        let Some(d) = &self.docker else {
            return "---".into();
        };
        let kind = match d.kind {
            1 => "Docker",
            UNKNOWN => "Docker or Podman",
            _ => "Podman",
        };
        let mut x = format!("{kind} version {}", html_escape(&d.version));
        if let Some(w) = &d.distro {
            let name = html_escape(&w.name);
            if w.runner_version == 0 {
                x.push_str(&format!("<br>WSL distro: {name}"));
            } else {
                x.push_str(&format!("<br>WSL distro: {name} ver {}", w.runner_version));
            }
        }
        if self.dont_use_wsl {
            x.push_str("<br>Config: don't use WSL");
        }
        if self.dont_use_docker {
            x.push_str("<br>Config: don't use Docker");
        }
        x
    }
}

/// `size_string` in `html/inc/util.inc`: 1024-based units, two decimals.
fn size_string(x: f64) -> String {
    use crate::site::number_format;
    if x > TERA {
        format!("{} TB", number_format(x / TERA, 2))
    } else if x > GIGA {
        format!("{} GB", number_format(x / GIGA, 2))
    } else if x > MEGA {
        format!("{} MB", number_format(x / MEGA, 2))
    } else if x > KILO {
        format!("{} KB", number_format(x / KILO, 2))
    } else {
        format!("{x} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stock_layout_is_read_like_host_inc() {
        let misc = r#"{"client_version":"8.2.4","gpus":[{"type":"nvidia","model":"NVIDIA GeForce RTX 3060 Ti","count":2,"ram_mb":8192,"driver_version":"57260","opencl_version":"300"}],
            "docker":{"version":"27.0.3","type":1,"wsl_distro":"boinc-buda-runner","boinc_buda_runner_version":9},
            "vbox":{"version":"7.1.6","hw_accel":true,"hw_accel_enabled":false},
            "config":{"dont_use_docker":true,"dont_use_wsl":false}}"#;
        let m = HostMisc::read(misc, "ignored");
        assert_eq!(m.boinc_version(), "8.2.4");
        assert_eq!(
            m.gpu_desc(),
            "2 NVIDIA GeForce RTX 3060 Ti; 8.00 GB RAM; Driver: 57260; OpenCL: 3.0"
        );
        assert_eq!(
            m.vbox_desc(),
            "Virtualbox 7.1.6<br>HW acceleration: yes<br>Enabled: no"
        );
        assert_eq!(
            m.docker_desc(),
            "Docker version 27.0.3<br>WSL distro: boinc-buda-runner ver 9<br>Config: don't use Docker"
        );
    }

    #[test]
    fn old_stock_serialnum_is_read_like_hostinfo_php() {
        let s = "[BOINC|8.2.4][CUDA|NVIDIA GeForce RTX 3060 Ti|1|8191MB|57260|300][vbox|7.1.6|1|1][docker|27.0.3|2]";
        let m = HostMisc::read("", s);
        assert_eq!(m.boinc_version(), "8.2.4");
        assert_eq!(
            m.gpu_desc(),
            "1 NVIDIA GeForce RTX 3060 Ti; 8.00 GB RAM; Driver: 57260; OpenCL: 3.0"
        );
        assert_eq!(
            m.vbox_desc(),
            "Virtualbox 7.1.6<br>HW acceleration: yes<br>Enabled: yes"
        );
        assert_eq!(m.docker_desc(), "Podman version 27.0.3");
    }

    #[test]
    fn this_servers_earlier_serialnum_is_read() {
        let s = "[BOINC|8.0.4][CUDA|2|8192MB][vbox|7.0<b>][docker|24]";
        let m = HostMisc::read("", s);
        assert_eq!(m.boinc_version(), "8.0.4");
        assert_eq!(m.gpu_desc(), "2 CUDA; 8.00 GB RAM");
        assert_eq!(m.vbox_desc(), "Virtualbox 7.0&lt;b&gt;");
        assert_eq!(m.docker_desc(), "Docker or Podman version 24");
        assert_eq!(HostMisc::read("", "").gpu_desc(), "---");
        assert_eq!(HostMisc::read("", "").boinc_version(), "---");
    }

    #[test]
    fn unusable_misc_falls_back_and_never_fails() {
        let s = "[BOINC|8.0.4]";
        for bad in [
            "",
            "  ",
            "{",
            "null",
            "[]",
            "5",
            "\"x\"",
            "{}",
            "{\"gpus\":",
            "\u{0}",
        ] {
            assert_eq!(HostMisc::read(bad, s).boinc_version(), "8.0.4", "{bad:?}");
        }
        // Wrong types inside a valid document read as PHP would.
        let odd = r#"{"client_version":8,"gpus":"x","docker":[],"vbox":5,"config":7}"#;
        let m = HostMisc::read(odd, s);
        assert_eq!(m.boinc_version(), "8");
        assert_eq!(m.gpu_desc(), "---");
        assert_eq!(m.docker_desc(), "---");
        let odd = r#"{"gpus":[1,null,{"model":["x"],"count":"3abc","ram_mb":"0","opencl_version":"abc"}]}"#;
        assert_eq!(HostMisc::read(odd, s).gpu_desc(), "3 ");
    }

    #[test]
    fn values_are_escaped() {
        let evil = "<img src=x onerror=alert(1)>\"'&";
        let misc = serde_json::json!({
            "client_version": evil,
            "gpus": [{"model": evil, "count": 1, "driver_version": evil}],
            "docker": {"version": evil, "type": 2, "wsl_distro": evil},
            "vbox": {"version": evil},
        })
        .to_string();
        let m = HostMisc::read(&misc, "");
        for s in [
            m.boinc_version(),
            m.gpu_desc(),
            m.vbox_desc(),
            m.docker_desc(),
            m.docker_desc(),
        ] {
            assert!(!s.contains("<img"), "{s}");
            assert!(!s.contains('"') && !s.contains('\''), "{s}");
        }
        // The serialnum layouts escape too.
        let s = format!("[BOINC|{evil}][CUDA|{evil}|1|8MB|{evil}|1][vbox|{evil}][docker|{evil}|1]");
        let m = HostMisc::read("", &s);
        for s in [
            m.boinc_version(),
            m.gpu_desc(),
            m.vbox_desc(),
            m.docker_desc(),
        ] {
            assert!(!s.contains("<img"), "{s}");
        }
    }

    #[test]
    fn a_wsl_distro_name_is_shown_as_stock_does_and_escaped() {
        let misc = r#"{"docker":{"version":"1","type":1,"wsl_distro":"alice-laptop"}}"#;
        assert_eq!(
            HostMisc::read(misc, "").docker_desc(),
            "Docker version 1<br>WSL distro: alice-laptop"
        );
        // Science United's nested form.
        let misc = r#"{"docker":{"version":"1","type":1,"wsl_distro":{"wsl_distro":"boinc-buda-runner","boinc_buda_runner_version":3}}}"#;
        assert_eq!(
            HostMisc::read(misc, "").docker_desc(),
            "Docker version 1<br>WSL distro: boinc-buda-runner ver 3"
        );
        let misc =
            serde_json::json!({"docker": {"version": "1", "type": 1, "wsl_distro": "<b>&\"'x"}})
                .to_string();
        assert_eq!(
            HostMisc::read(&misc, "").docker_desc(),
            "Docker version 1<br>WSL distro: &lt;b&gt;&amp;&quot;&#39;x"
        );
    }

    #[test]
    fn sizes_are_stocks() {
        assert_eq!(size_string(12124.0 * MEGA), "11.84 GB");
        assert_eq!(size_string(512.0 * MEGA), "512.00 MB");
        assert_eq!(size_string(1024.0 * MEGA), "1,024.00 MB");
        assert_eq!(size_string(2.0 * TERA), "2.00 TB");
        assert_eq!(size_string(10.0), "10 bytes");
    }
}
