//! Homogeneous redundancy classes: `sched/hr.cpp` and `sched/sched_hr.h`.
//!
//! A host's class is `os + cpu`, both taken from the host's OS name, CPU
//! vendor and CPU model as the client reports them. An app that uses
//! homogeneous redundancy (HR) sends the results of a workunit only to hosts
//! of one class, so that the results are computed on numerically alike
//! machines. The type says how finely hosts are told apart: 1 is "fine" (OS
//! and CPU family), 2 is "coarse" (OS and CPU vendor). Hosts the type cannot
//! place get no work from the app.

use crate::model::App;
use crate::Config;

/// Number of HR types plus one (`HR_NTYPES` in `sched/hr.h`): 0 is "no HR".
pub const HR_NTYPES: i32 = 3;

// CPU types, `const int` in `sched/hr.cpp`.
const NOCPU: i32 = 1;
const INTEL: i32 = 2;
const AMD: i32 = 3;
const MACINTOSH: i32 = 4;
const AMD_ATHLON: i32 = 5;
const AMD_DURON: i32 = 6;
const AMD_SEMPRON: i32 = 7;
const AMD_OPTERON: i32 = 8;
const AMD_ATHLON64: i32 = 9;
const AMD_ATHLON_XP: i32 = 10;
const INTEL_XEON: i32 = 11;
const INTEL_CELERON: i32 = 12;
const INTEL_PENTIUM: i32 = 13;
const INTEL_PENTIUM_II: i32 = 14;
const INTEL_PENTIUM_III: i32 = 15;
const INTEL_PENTIUM4: i32 = 16;
const INTEL_PENTIUM_D: i32 = 17;
const INTEL_PENTIUM_M: i32 = 18;
const AMD_ATHLON_MP: i32 = 19;
const AMD_TURION: i32 = 20;
const INTEL_CORE2: i32 = 21;
const ARM: i32 = 22;

// Operating systems.
const NOOS: i32 = 128;
const LINUX: i32 = 256;
const WINDOWS: i32 = 384;
const DARWIN: i32 = 512;
const FREEBSD: i32 = 640;
const ANDROID: i32 = 768;

/// The fields of a host that decide its class.
#[derive(Debug, Clone, Copy)]
pub struct HrHost<'a> {
    pub os_name: &'a str,
    pub p_vendor: &'a str,
    pub p_model: &'a str,
}

/// `strcasestr`: whether `hay` contains `needle`, ignoring ASCII case.
fn has(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    n.is_empty() || h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

fn os(h: &HrHost<'_>) -> i32 {
    if has(h.os_name, "Linux") {
        LINUX
    } else if has(h.os_name, "Windows") {
        WINDOWS
    } else if has(h.os_name, "Darwin") {
        DARWIN
    } else if has(h.os_name, "FreeBSD") {
        FREEBSD
    } else if has(h.os_name, "Android") {
        ANDROID
    } else {
        NOOS
    }
}

fn cpu_coarse(h: &HrHost<'_>) -> i32 {
    if has(h.p_vendor, "Intel") {
        return INTEL;
    }
    // Upstream keeps AMD in the Intel class (`//if (strcasestr(host.p_vendor, "AMD")) return AMD;`).
    if has(h.p_vendor, "AMD") {
        return INTEL;
    }
    if has(h.p_vendor, "Macintosh") {
        return MACINTOSH;
    }
    if has(h.p_vendor, "ARM") {
        return ARM;
    }
    // Sometimes the vendor is blank and the model says ARM; `strstr` is
    // case sensitive.
    if h.p_model.contains("ARM") {
        return ARM;
    }
    NOCPU
}

fn cpu_fine(h: &HrHost<'_>) -> i32 {
    let (vendor, model) = (h.p_vendor, h.p_model);
    if has(vendor, "Intel") {
        if has(model, "Xeon") {
            return INTEL_XEON;
        }
        if has(model, "Celeron") {
            if has(model, " M ") {
                return INTEL_PENTIUM_M;
            }
            if has(model, " D ") {
                return INTEL_PENTIUM_D;
            }
            if has(model, "III") {
                return INTEL_PENTIUM_III;
            }
            return INTEL_CELERON;
        }
        if has(model, "Core") {
            return INTEL_CORE2;
        }
        if has(model, "Pentium") {
            if has(model, "III") {
                return INTEL_PENTIUM_III;
            }
            if has(model, "II") {
                return INTEL_PENTIUM_II;
            }
            if has(model, " 4 ") {
                return INTEL_PENTIUM4;
            }
            if has(model, " D ") {
                return INTEL_PENTIUM_D;
            }
            if has(model, " M ") {
                return INTEL_PENTIUM_M;
            }
            return INTEL_PENTIUM;
        }
        if has(model, "x86") {
            // Checked in this order: "Family 6 Model 1" also matches
            // "Family 6 Model 10" and "... 11", so the order decides.
            const FAMILIES: [(&str, i32); 13] = [
                ("Family 6 Model 6", INTEL_CELERON),
                ("Family 6 Model 9", INTEL_PENTIUM_M),
                ("Family 6 Model 10", INTEL_XEON),
                ("Family 5 Model 1", INTEL_PENTIUM),
                ("Family 5 Model 2", INTEL_PENTIUM),
                ("Family 6 Model 1", INTEL_PENTIUM),
                ("Family 15 Model 1", INTEL_PENTIUM4),
                ("Family 15 Model 2", INTEL_PENTIUM4),
                ("Family 6 Model 7", INTEL_PENTIUM_III),
                ("Family 6 Model 8", INTEL_PENTIUM_III),
                ("Family 6 Model 11", INTEL_PENTIUM_III),
                ("Family 6 Model 3", INTEL_PENTIUM_II),
                ("Family 6 Model 5", INTEL_PENTIUM_II),
            ];
            for (text, class) in FAMILIES {
                if has(model, text) {
                    return class;
                }
            }
        }
        return INTEL;
    }
    if has(vendor, "AMD") {
        if has(model, "Duron") {
            return AMD_DURON;
        }
        if has(model, "Opteron") {
            return AMD_OPTERON;
        }
        if has(model, "Sempron") {
            return AMD_SEMPRON;
        }
        if has(model, "Turion") {
            return AMD_TURION;
        }
        if has(model, "Athlon") {
            if has(model, "XP") {
                return AMD_ATHLON_XP;
            }
            if has(model, "MP") {
                return AMD_ATHLON_MP;
            }
            if has(model, "64") {
                return AMD_ATHLON64;
            }
            return AMD_ATHLON;
        }
        return AMD;
    }
    if has(vendor, "Macintosh") {
        return MACINTOSH;
    }
    if has(vendor, "ARM") {
        return ARM;
    }
    NOCPU
}

/// `hr_class` in `sched/hr.cpp`: the class of a host under an HR type, 0
/// for a type that is not 1 or 2. Meaningful only when [`hr_unknown_class`]
/// is false.
pub fn hr_class(host: &HrHost<'_>, hr_type: i32) -> i32 {
    match hr_type {
        1 => os(host) + cpu_fine(host),
        2 => os(host) + cpu_coarse(host),
        _ => 0,
    }
}

/// `hr_unknown_class` in `sched/hr.cpp`: whether the type cannot place the
/// host, so that the app sends it nothing. Coarse HR places only Windows and
/// Linux hosts, and Darwin hosts with an Intel (or AMD) or PowerPC CPU.
pub fn hr_unknown_class(host: &HrHost<'_>, hr_type: i32) -> bool {
    match hr_type {
        1 => os(host) == NOOS || cpu_fine(host) == NOCPU,
        2 => match os(host) {
            WINDOWS | LINUX => false,
            DARWIN => !matches!(cpu_coarse(host), INTEL | MACINTOSH),
            _ => true,
        },
        _ => false,
    }
}

/// `app_hr_type` in `sched/sched_hr.h`: the app's own type, else the
/// project's `<homogeneous_redundancy>`; 0 is no HR.
pub fn app_hr_type(app: &App, config: &Config) -> i32 {
    if app.homogeneous_redundancy != 0 {
        app.homogeneous_redundancy
    } else {
        config.homogeneous_redundancy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host<'a>(os_name: &'a str, p_vendor: &'a str, p_model: &'a str) -> HrHost<'a> {
        HrHost {
            os_name,
            p_vendor,
            p_model,
        }
    }

    /// `(os name, vendor, model, fine class, coarse class)`, each worked out
    /// by hand from `sched/hr.cpp`. A class is `os + cpu`.
    #[test]
    fn classes_follow_hr_cpp() {
        let table: [(&str, &str, &str, i32, i32); 24] = [
            // Linux and Windows hosts, modern CPUs.
            (
                "Linux",
                "GenuineIntel",
                "Intel(R) Xeon(R) CPU E5-2680 v4",
                256 + 11,
                256 + 2,
            ),
            (
                "Linux",
                "GenuineIntel",
                "Intel(R) Core(TM) i7-9700K CPU @ 3.60GHz",
                256 + 21,
                256 + 2,
            ),
            (
                "Microsoft Windows 11",
                "GenuineIntel",
                "Intel(R) Celeron(R) N4020",
                384 + 12,
                384 + 2,
            ),
            // AMD is "AMD" for the fine type and Intel for the coarse one.
            (
                "Linux",
                "AuthenticAMD",
                "AMD Ryzen 9 5950X 16-Core Processor",
                256 + 3,
                256 + 2,
            ),
            (
                "Microsoft Windows 10",
                "AuthenticAMD",
                "AMD EPYC 7763 64-Core Processor",
                384 + 3,
                384 + 2,
            ),
            (
                "Linux",
                "AuthenticAMD",
                "AMD Opteron(tm) Processor 6174",
                256 + 8,
                256 + 2,
            ),
            (
                "Linux",
                "AuthenticAMD",
                "AMD Athlon(tm) 64 X2 Dual Core",
                256 + 9,
                256 + 2,
            ),
            (
                "Linux",
                "AuthenticAMD",
                "AMD Athlon(tm) XP 2800+",
                256 + 10,
                256 + 2,
            ),
            (
                "Linux",
                "AuthenticAMD",
                "AMD Athlon(tm) MP 2000+",
                256 + 19,
                256 + 2,
            ),
            (
                "Linux",
                "AuthenticAMD",
                "AMD Athlon(tm) II X4",
                256 + 5,
                256 + 2,
            ),
            (
                "Linux",
                "AuthenticAMD",
                "AMD Turion(tm) 64 X2",
                256 + 20,
                256 + 2,
            ),
            (
                "Linux",
                "AuthenticAMD",
                "AMD Sempron(tm) 3000+",
                256 + 7,
                256 + 2,
            ),
            ("Linux", "AuthenticAMD", "AMD Duron(tm)", 256 + 6, 256 + 2),
            // Old Intel models.
            (
                "Linux",
                "GenuineIntel",
                "Intel(R) Pentium(R) 4 CPU 3.00GHz",
                256 + 16,
                256 + 2,
            ),
            (
                "Linux",
                "GenuineIntel",
                "Intel(R) Pentium(R) III CPU",
                256 + 15,
                256 + 2,
            ),
            (
                "Linux",
                "GenuineIntel",
                "Intel(R) Pentium(R) D CPU 3.40GHz",
                256 + 17,
                256 + 2,
            ),
            (
                "Linux",
                "GenuineIntel",
                "x86 Family 6 Model 10 Stepping 4",
                256 + 11,
                256 + 2,
            ),
            (
                "Linux",
                "GenuineIntel",
                "x86 Family 15 Model 2 Stepping 7",
                256 + 16,
                256 + 2,
            ),
            // macOS: Intel and PowerPC are placed, Apple silicon is not.
            (
                "Darwin",
                "GenuineIntel",
                "Intel(R) Core(TM) i5-8259U",
                512 + 21,
                512 + 2,
            ),
            ("Darwin", "Macintosh", "PowerPC G5", 512 + 4, 512 + 4),
            // ARM on Linux: placed by the fine type and the coarse one.
            ("Linux", "ARM", "Cortex-A72", 256 + 22, 256 + 22),
            (
                "Linux",
                "",
                "ARMv8 Processor rev 1 (v8l)",
                256 + 1,
                256 + 22,
            ),
            ("Android", "ARM", "AArch64", 768 + 22, 768 + 22),
            (
                "FreeBSD",
                "GenuineIntel",
                "Intel(R) Xeon(R) CPU",
                640 + 11,
                640 + 2,
            ),
        ];
        for (os_name, vendor, model, fine, coarse) in table {
            let h = host(os_name, vendor, model);
            assert_eq!(hr_class(&h, 1), fine, "fine: {os_name} {vendor} {model}");
            assert_eq!(
                hr_class(&h, 2),
                coarse,
                "coarse: {os_name} {vendor} {model}"
            );
        }
    }

    #[test]
    fn unknown_hosts_follow_hr_cpp() {
        // Fine: an OS it does not name, or a CPU it cannot tell.
        assert!(hr_unknown_class(
            &host("Solaris", "GenuineIntel", "Xeon"),
            1
        ));
        assert!(hr_unknown_class(&host("", "GenuineIntel", "Xeon"), 1));
        assert!(hr_unknown_class(&host("Linux", "RISC-V", "SiFive U74"), 1));
        assert!(hr_unknown_class(&host("Linux", "", ""), 1));
        assert!(!hr_unknown_class(&host("Linux", "GenuineIntel", "x"), 1));
        // Fine places a Mac with an ARM CPU ("Apple M1" reports vendor Apple
        // on some clients, which it cannot place, and ARM on others).
        assert!(hr_unknown_class(&host("Darwin", "Apple", "Apple M1"), 1));
        assert!(!hr_unknown_class(&host("Darwin", "ARM", "Apple M1"), 1));
        // Coarse: only Windows, Linux and Intel or PowerPC Darwin.
        assert!(!hr_unknown_class(&host("Linux", "RISC-V", "x"), 2));
        assert!(!hr_unknown_class(&host("Windows 10", "", ""), 2));
        assert!(!hr_unknown_class(&host("Darwin", "AuthenticAMD", "x"), 2));
        assert!(!hr_unknown_class(&host("Darwin", "Macintosh", "x"), 2));
        assert!(hr_unknown_class(&host("Darwin", "ARM", "Apple M1"), 2));
        assert!(hr_unknown_class(&host("Darwin", "Apple", "Apple M1"), 2));
        assert!(hr_unknown_class(&host("Android", "ARM", "x"), 2));
        assert!(hr_unknown_class(&host("FreeBSD", "GenuineIntel", "x"), 2));
        assert!(hr_unknown_class(&host("", "GenuineIntel", "x"), 2));
        // No HR, or a type it does not know: nothing is unknown, class 0.
        assert!(!hr_unknown_class(&host("", "", ""), 0));
        assert!(!hr_unknown_class(&host("", "", ""), 3));
        assert_eq!(hr_class(&host("Linux", "GenuineIntel", "Xeon"), 0), 0);
        assert_eq!(hr_class(&host("Linux", "GenuineIntel", "Xeon"), 3), 0);
    }

    #[test]
    fn matching_ignores_case_except_for_arm_in_the_model() {
        assert_eq!(
            hr_class(&host("LINUX", "genuineintel", "xeon"), 1),
            256 + 11
        );
        assert_eq!(hr_class(&host("linux", "", "armv7"), 2), 256 + 1);
        assert_eq!(hr_class(&host("linux", "", "ARMv7"), 2), 256 + 22);
        // The model of an Intel host is matched in order: Xeon before Core.
        assert_eq!(
            hr_class(&host("Linux", "GenuineIntel", "Core Xeon"), 1),
            256 + 11
        );
        // Celeron variants.
        assert_eq!(
            hr_class(&host("Linux", "GenuineIntel", "Celeron M 1.5"), 1),
            256 + 18
        );
        assert_eq!(
            hr_class(&host("Linux", "GenuineIntel", "Celeron D 2.4"), 1),
            256 + 17
        );
        assert_eq!(
            hr_class(&host("Linux", "GenuineIntel", "Celeron III"), 1),
            256 + 15
        );
        // Pentium: III before II, which is a substring of III.
        assert_eq!(
            hr_class(&host("Linux", "GenuineIntel", "Pentium II"), 1),
            256 + 14
        );
        assert_eq!(
            hr_class(&host("Linux", "GenuineIntel", "Pentium M"), 1),
            256 + 13
        );
    }

    #[test]
    fn the_app_type_overrides_the_project_type() {
        let mut config = Config::default();
        let mut app = App::default();
        assert_eq!(app_hr_type(&app, &config), 0);
        config.homogeneous_redundancy = 2;
        assert_eq!(app_hr_type(&app, &config), 2);
        app.homogeneous_redundancy = 1;
        assert_eq!(app_hr_type(&app, &config), 1);
    }
}
