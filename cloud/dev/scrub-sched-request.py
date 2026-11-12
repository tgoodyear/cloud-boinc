#!/usr/bin/env python3
"""Scrub a scheduler request captured from a stock client so it can be kept in the repository.

    dev/scrub-sched-request.py --input FILE --output FILE [--project-url URL]
                               [--check-only] [--forbid WORD ...]

The client writes its latest request to `sched_request_<project>.xml` in its data directory. That
file holds the account key, the host's cross-project ID and CPID, its domain name and address, the
kernel and CPU model, and the project's URL and code-signing key. `--input` is such a file;
`--output` gets the same document with those values replaced by fixed placeholders. Element names,
numbers, counts, task names and timing are left as they were. The CPU model and feature list are
replaced by a placeholder of the same kind as the request's platform (x86, 64-bit ARM, 32-bit ARM,
Apple silicon), so that a fixture keeps the shape the client sends.

After scrubbing, the output is checked, and nothing is written if a check fails:

  * no run of 32 or more hexadecimal digits (keys, CPIDs, MD5 sums);
  * no IPv4 address outside the documentation ranges of RFC 5737;
  * no MAC address (six hex pairs joined by colons or dashes), whatever element it is in;
  * none of the words given with `--forbid` (for example the capture machine's host name), and none
    of the built-in ones (`usnewsmap`, `/root/`, `/home/`, `\\Users\\`, `/Users/`).

`--check-only` runs the checks on `--input` itself, without changing it, which is how the committed
fixtures are verified (see `cloud/tests/client-corpus/README.md`).

The script reads and writes text; it does not parse the XML, so a malformed request stays
malformed, byte for byte, except for the replaced values.
"""

import argparse
import re
import sys

PLACEHOLDER_URL = "https://project.example/"

# Elements whose whole text is replaced. The value is what stays; an empty element stays empty,
# because the parser treats "no value" and "a value" differently for some of them.
TEXT_ELEMENTS = {
    "authenticator": "fixture-authenticator",
    "cross_project_id": "fixture-cross-project-id",
    "host_cpid": "fixture-host-cpid",
    "global_prefs_source_email_hash": "fixture-email-hash",
    "domain_name": "fixture-host",
    "ip_addr": "192.0.2.1",
    "p_model": "Fixture CPU @ 3.00GHz",
    "p_features": "fpu vme de pse tsc msr pae mce cx8 apic sep mtrr pge mca cmov pat pse36 clflush mmx fxsr sse sse2 ht syscall nx lm constant_tsc rep_good nopl pni ssse3 cx16 sse4_1 sse4_2 popcnt avx avx2",
    "product_name": "fixture-product",
    # The offset of the capture machine's time zone from UTC, in seconds.
    "timezone": "0",
    "serialnum": "fixture-serial",
    "md5_cksum": "fixture-md5",
    # `HOST_INFO::write` in lib/hostinfo.cpp (the client has no `mac_addr`).
    "mac_address": "fixture-mac",
    # Written by older clients' project and account sections, and by account manager requests.
    "email_hash": "fixture-email-hash",
    "external_cpid": "fixture-external-cpid",
    "user_name": "fixture-user",
    "team_name": "fixture-team",
    "project_name": "fixture-project",
    "client_brand": "fixture-brand",
}

# `p_model` and `p_features` by the kind of platform (`HOST_INFO::write` in lib/hostinfo.cpp; the
# client fills them from /proc/cpuinfo, `sysctl` or the registry). The values name no real CPU:
# the ARM ones are what qemu-user's emulated CPU reports, the Apple one an M1's generic list.
CPU_PLACEHOLDERS = [
    # (a prefix of `<platform_name>`, p_model, p_features). Prefixes, not substrings: Intel Macs
    # (`x86_64-apple-darwin`) and 64-bit ARM Macs (`arm64-apple-darwin`) need different ones, and
    # `arm-` must not match `arm64-`. Other platforms get the x86 placeholders.
    (
        "arm64-apple-darwin",
        "Apple Fixture",
        "amx_version_1 arm64 armv8_1_atomics armv8_2_fhm armv8_2_sha3 armv8_2_sha512 armv8_crc32 floatingpoint neon neon_fp16 neon_hpfp ucnormal_mem",
    ),
    (
        "aarch64-",
        "ARMv8 Processor rev 0 (v8l) [Impl 0x00 Arch 8 Variant 0x0 Part 0x051 Rev 0]",
        "fp asimd aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm jscvt fcma lrcpc dcpop sha3 sm3 sm4 asimddp sha512 sve asimdfhm dit uscat ilrcpc flagm sb paca pacg dcpodp sve2 sveaes svepmull svebitperm svesha3 svesm4 flagm2 frint svei8mm svef32mm svef64mm svebf16 i8mm bf16 rng bti mte mte3 sme smei16i64 smef64f64 smei8i32 smef16f32 smeb16f32 smef32f32 smefa64 mops hbc",
    ),
    (
        "arm-",
        "ARMv8 Processor rev 0 (v8l) [Impl 0x41 Arch 8 Variant 0x1 Part 0xd07 Rev 0]",
        "swp half thumb fast_mult vfp edsp neon vfpv3 tls vfpv4 idiva idivt vfpd32 lpae fphp asimdhp asimddp asimdfhm asimdbf16 i8mm aes pmull sha1 sha2 crc32 sb ssbs",
    ),
]

# Elements that name a device or a distribution, replaced only inside the block that holds them,
# because `name` means something else elsewhere in a request (task, file and platform names).
#   coprocs: GPU names and models (`COPROC_*::write_xml` in lib/coproc.cpp), the OpenCL device
#       name (`OPENCL_DEVICE_PROP::write_xml`), PCI bus, device and domain numbers
#   opencl_cpu_prop: the OpenCL CPU device name (`OPENCL_CPU_PROP::write_xml`)
#   wsl: the names of the Windows Subsystem for Linux distributions (`WSL_DISTRO::write_xml`)
BLOCK_ELEMENTS = {
    "coprocs": {
        "name": "Fixture GPU",
        "model": "Fixture GPU",
        "bus_id": "0",
        "device_id": "0",
        "domain_id": "0",
    },
    "opencl_cpu_prop": {"name": "Fixture CPU"},
    "wsl": {"distro_name": "fixture-distro"},
}

# Six pairs of hex digits joined by colons or by dashes.
MAC = re.compile(r"(?<![0-9A-Fa-f])[0-9A-Fa-f]{2}([:-])(?:[0-9A-Fa-f]{2}\1){4}[0-9A-Fa-f]{2}(?![0-9A-Fa-f])")
ALLOWED_V4 = re.compile(r"^(192\.0\.2\.|198\.51\.100\.|203\.0\.113\.)")
HEX32 = re.compile(r"[0-9a-fA-F]{32,}")
IPV4 = re.compile(r"(?<![0-9.])(?:[0-9]{1,3}\.){3}[0-9]{1,3}(?![0-9])")
BUILTIN_FORBIDDEN = ["usnewsmap", "/root/", "/home/", "\\users\\", "/users/"]


def scrub_block(text, block, elements):
    def inside(m):
        body = m.group(0)
        for tag, value in elements.items():
            body = re.sub(
                r"(<%s>)[^<]+(</%s>)" % (tag, tag),
                lambda n, v=value: n.group(1) + v + n.group(2),
                body,
            )
        return body

    return re.sub(r"(?s)<%s>.*?</%s>" % (block, block), inside, text)


def cpu_placeholders(text):
    """The `p_model` and `p_features` placeholders for the platform the request names."""
    m = re.search(r"<platform_name>([^<]*)</platform_name>", text)
    platform = m.group(1) if m else ""
    for prefix, model, features in CPU_PLACEHOLDERS:
        if platform.startswith(prefix):
            return model, features
    return TEXT_ELEMENTS["p_model"], TEXT_ELEMENTS["p_features"]


def scrub(text, project_url):
    model, features = cpu_placeholders(text)
    for block, elements in BLOCK_ELEMENTS.items():
        text = scrub_block(text, block, elements)
    for tag, value in TEXT_ELEMENTS.items():
        value = {"p_model": model, "p_features": features}.get(tag, value)
        # `[^<]+` leaves an empty element empty.
        text = re.sub(
            r"(<%s>)[^<]+(</%s>)" % (tag, tag),
            lambda m, v=value: m.group(1) + v + m.group(2),
            text,
        )
    # The project's URLs (source of the preferences, upload URL of an output file).
    text = re.sub(r"https?://[A-Za-z0-9.-]+(?::[0-9]+)?/", project_url, text)
    # The kernel release in the OS version, which names the host's distribution kernel.
    text = re.sub(r"\[[^|\]\n]*\|libc", "[6.12.0|libc", text)
    # Absolute paths a task's stderr may carry.
    text = re.sub(r"/(?:root|home|var/lib|opt|usr)/[^\s<\"']*", "/var/lib/boinc/path", text)
    # The same on Windows: a task's stderr may name a user's profile or the data directory.
    text = re.sub(
        r"[A-Za-z]:\\(?:Users|ProgramData|Documents and Settings)\\[^\s<\"']*",
        lambda m: "C:\\ProgramData\\BOINC\\path",
        text,
    )
    # The code-signing key the client echoes back: key size, then hex lines, then a dot.
    text = re.sub(
        r"(<code_sign_key>\s*\n)\s*[0-9]+\s*\n(?:[0-9a-fA-F]+\s*\n)+(\.\s*\n</code_sign_key>)",
        lambda m: m.group(1) + "1024\nfixture-code-sign-key\n" + m.group(2),
        text,
    )
    return text


def check(text, forbid):
    """Returns a list of what is wrong with `text`."""
    problems = []
    for m in HEX32.finditer(text):
        problems.append("a run of %d hex digits: %s..." % (len(m.group(0)), m.group(0)[:6]))
    for m in MAC.finditer(text):
        problems.append("a MAC address: %s" % m.group(0))
    for m in IPV4.finditer(text):
        if not ALLOWED_V4.match(m.group(0)):
            problems.append("IPv4 address %s" % m.group(0))
    for word in BUILTIN_FORBIDDEN + forbid:
        if word and word.lower() in text.lower():
            problems.append("forbidden text %r" % word)
    return problems


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--input", required=True)
    ap.add_argument("--output")
    ap.add_argument("--project-url", default=PLACEHOLDER_URL)
    ap.add_argument("--check-only", action="store_true")
    ap.add_argument("--forbid", action="append", default=[])
    args = ap.parse_args()
    if not args.check_only and not args.output:
        ap.error("--output is required unless --check-only")

    with open(args.input, encoding="utf-8") as f:
        text = f.read()
    if not args.check_only:
        text = scrub(text, args.project_url)
    problems = check(text, args.forbid)
    if problems:
        for p in problems:
            print("%s: %s" % (args.input, p), file=sys.stderr)
        return 1
    if not args.check_only:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
