#!/usr/bin/env python3
"""Turns what capture.sh recorded into the request corpus.

    build-corpus.py --capture DIR --out DIR

For each captured scenario it writes OUT/<scenario>/steps.json and the
request files. The scheduler requests of the stock client are kept as the
client sent them, except for the values that identify the machine, which are
replaced by fixed test values. Three more scenarios are built from the
captured requests: `uploads` and `lost` change one field of a captured
request, `web` is a list of account manager and site requests, written by
hand because no stock client sends them. Replies are never written here:
replay.py asks the stock server for them.

Step order is the order the proxy answered the requests, which is the
order the client sent the ones that depend on each other.
"""
import argparse
import json
import os
import re
import shutil
import socket
import urllib.parse

from names import Names

ap = argparse.ArgumentParser()
ap.add_argument("--capture", required=True)
ap.add_argument("--out", required=True)
ap.add_argument("--scenarios", default="attach work report anon lost quick uploads web",
                help="the scenarios to write; the others are left as they are")
args = ap.parse_args()

AUTH = "5a1f0c3b9d7e42a6b8c0d1e2f3a4b5c6"
FIXED_CPID = "0123456789abcdef0123456789abcdef"
# Replacements for what identifies the machine: tag -> new value.
HOST_FIELDS = {
    "domain_name": "test-host",
    "ip_addr": "192.0.2.10",
    "host_cpid": FIXED_CPID,
    "p_vendor": "GenuineTest",
    "p_model": "Test CPU @ 3.00GHz",
    "p_features": "fpu sse2",
    "os_name": "Linux Debian",
    "os_version": "Debian GNU/Linux 12 (bookworm) [6.1.0-test|libc 2.36]",
    "m_nbytes": "8589934592.000000",
    "m_swap": "2147483648.000000",
    "d_total": "107374182400.000000",
    "d_free": "53687091200.000000",
    "serialnum": "",
    "product_name": "",
    "virtualbox_version": "",
}


def machine_words():
    """What names the machine this runs on, which is the one the client ran on."""
    words = {socket.gethostname(), socket.gethostname().split(".")[0]}
    try:
        for line in open("/proc/cpuinfo"):
            if line.startswith("model name"):
                words.add(line.split(":", 1)[1].strip().split(" @ ")[0])
    except OSError:
        pass
    return sorted(w for w in words if len(w) > 3 and w != "localhost")


MACHINE_WORDS = machine_words()


def sanitize_sched(text):
    for tag, value in HOST_FIELDS.items():
        text = re.sub(r"<%s>[^<]*</%s>" % (tag, tag), "<%s>%s</%s>" % (tag, value, tag), text)
    # Paths of the machine the client ran on, in stderr texts.
    text = re.sub(r"/(root|home)/[^\s<\"]*", "/var/lib/boinc/slots/0", text)
    for word in MACHINE_WORDS:
        assert word not in text, "unsanitized %s in a scheduler request" % word
    return text


def read_exchange(d, n):
    raw = open(os.path.join(d, "%04d.request" % n), "rb").read()
    head, _, body = raw.partition(b"\n\n")
    line = head.split(b"\n")[0].decode()
    method, path = line.split(" ", 1)
    return method, path, body


class Scenario:
    def __init__(self, name):
        self.name = name
        self.dir = os.path.join(args.out, name)
        shutil.rmtree(self.dir, ignore_errors=True)
        os.makedirs(self.dir)
        self.steps = []

    def add(self, kind, label, body=None, ext="request", **extra):
        step = {"name": "%02d-%s" % (len(self.steps) + 1, label), "kind": kind}
        if body is not None:
            fn = "%s.%s" % (step["name"], ext)
            with open(os.path.join(self.dir, fn), "wb") as f:
                f.write(body)
            step["request"] = fn
        step.update(extra)
        self.steps.append(step)
        return step

    def finish(self):
        with open(os.path.join(self.dir, "steps.json"), "w") as f:
            json.dump(self.steps, f, indent=2)
            f.write("\n")


def captured(scenario):
    d = os.path.join(args.capture, scenario)
    out = []
    for line in open(os.path.join(d, "exchanges.jsonl")):
        e = json.loads(line)
        out.append((e["n"], d))
    return sorted(out)


def kept(scenario):
    """The exchanges of a captured scenario that become steps."""
    for n, d in captured(scenario):
        method, path, body = read_exchange(d, n)
        yield n, d, method, path, body


def web_label(path):
    return re.sub(r"\.php$", "", path.split("?")[0].split("/")[-1]) or "master"


def canonical_sched(scenario):
    """The scheduler requests of a captured scenario with the jobs named by
    the order the client was sent them, as (exchange number, text)."""
    names = Names()
    out = []
    for n, d, method, path, body in kept(scenario):
        if path != "/diff_cgi/cgi":
            continue
        out.append((n, sanitize_sched(names.to_canon(body.decode()))))
        reply = open(os.path.join(d, "%04d.reply" % n), "rb").read().decode()
        names.learn(reply)
    return out


def build_captured(scenario):
    sc = Scenario(scenario)
    canon = dict(canonical_sched(scenario))
    for n, d, method, path, body in kept(scenario):
        if path == "/diff_cgi/cgi":
            sc.add("sched", "sched", canon[n].encode(), "request.xml")
        elif path == "/diff_cgi/file_upload_handler":
            sc.add("upload", "upload", body)
        elif path == "/diff/":
            sc.add("web", "master", method=method, url="", compare="master")
        elif re.match(r"/diff/(get_project_config|lookup_account|create_account)\.php", path):
            sc.add("web", web_label(path), method=method, url=path[len("/diff/"):])
        # Downloads and the notices feed are not replayed.
    sc.finish()
    return sc


def requests(scenario, kind):
    if kind == "sched":
        return [(n, None, text.encode()) for n, text in canonical_sched(scenario)]
    return [(n, d, body) for n, d, m, p, body in kept(scenario)
            if p == "/diff_cgi/file_upload_handler"]


def upload_name(body):
    return re.search(rb"<name>([^<]*)</name>", body).group(1).decode()


def build_uploads():
    sc = Scenario("uploads")
    ups = requests("work", "upload")
    body_a, body_b, body_c = ups[0][2], ups[1][2], ups[2][2]
    name_a, name_b, name_c = upload_name(body_a), upload_name(body_b), upload_name(body_c)

    def size_query(name):
        return (
            "<data_server_request>\n    <core_client_major_version>8</core_client_major_version>\n"
            "    <core_client_minor_version>0</core_client_minor_version>\n"
            "    <core_client_release>4</core_client_release>\n"
            "    <get_file_size>%s</get_file_size>\n</data_server_request>\n" % name
        ).encode()

    def data_at(body):
        return body.index(b"<data>\n") + len(b"<data>\n")

    query = ("a get_file_size request in the form of FILE_XFER::init_upload in the client's "
             "file_xfer.cpp")
    sc.add("upload", "size-before", size_query(name_a), derived=query + ", for a file not yet uploaded")
    sc.add("upload", "offset-beyond", re.sub(rb"<offset>\d+</offset>", b"<offset>10</offset>", body_a, count=1),
           derived="an offset for a file of which the server holds nothing")
    sc.add("upload", "upload", body_a)
    sc.add("upload", "size-after", size_query(name_a), derived=query + ", for the file just uploaded")
    sc.add("upload", "upload-again", body_a, derived="the same request sent a second time, as a "
           "client does after a lost reply")
    # Same name, other contents: change the data and its checksum field.
    changed = body_a[:data_at(body_a)] + b"X" + body_a[data_at(body_a) + 1:]
    changed = re.sub(rb"<md5_cksum>[0-9a-f]{32}", b"<md5_cksum>" + b"0" * 32, changed, count=1)
    sc.add("upload", "other-contents", changed, derived="the data and the checksum changed, same file name")
    sig = re.search(rb"<xml_signature>\n([0-9a-f])", body_b)
    flipped = body_b[:sig.start(1)] + (b"0" if sig.group(1) != b"0" else b"1") + body_b[sig.end(1):]
    sc.add("upload", "bad-signature", flipped, derived="the first digit of the upload certificate changed")
    sc.add("upload", "other-name", body_b.replace(name_b.encode(), name_b.encode().replace(b"_r", b"_q", 1)),
           derived="the file name changed, the certificate kept")
    sc.add("upload", "too-large", re.sub(rb"<nbytes>\d+</nbytes>", b"<nbytes>6000000</nbytes>", body_b, count=1),
           derived="nbytes above the certificate's max_nbytes")
    # An interrupted upload and its resumption: the first ten bytes, then the rest from offset 10.
    cut = body_c[:data_at(body_c) + 10]
    sc.add("upload", "resume-part-1", cut, derived="the request cut after ten bytes of data, as when a "
           "connection drops")
    sc.add("upload", "resume-size", size_query(name_c), derived=query + ", after the interrupted upload")
    rest = re.sub(rb"<offset>\d+</offset>", b"<offset>10</offset>", body_c, count=1)
    rest = rest[:data_at(rest)] + body_c[data_at(body_c) + 10:]
    sc.add("upload", "resume-part-2", rest, derived="the rest of the data, from offset 10")
    sc.add("upload", "no-data-tag", b"<data_server_request>\n<file_upload>\n</file_upload>\n</data_server_request>\n",
           derived="a request without a file_info or data")
    sc.add("upload", "garbage", b"hello\n", derived="not a request")
    sc.finish()


def build_lost():
    sc = build_captured("lost")
    reqs = requests("lost", "sched")
    second = reqs[1][2].decode()
    assert "<other_result>" in second
    cut = re.sub(r"<other_results>.*?</other_results>", "<other_results>\n</other_results>", second,
                 flags=re.S)
    m = re.search(r"<rpc_seqno>(\d+)</rpc_seqno>", second)
    cut = cut.replace(m.group(0), "<rpc_seqno>%d</rpc_seqno>" % (int(m.group(1)) + 1))
    sc.add("sched", "sched-forgot-tasks", sanitize_sched(cut).encode(), "request.xml",
           derived="the previous request with other_results emptied and rpc_seqno one higher: "
           "a client that lost its tasks")
    sc.finish()


def build_quick():
    sc = Scenario("quick")
    first = requests("work", "sched")[0][2].decode()
    sc.add("sched", "sched-first", sanitize_sched(first).encode(), "request.xml")
    m = re.search(r"<rpc_seqno>(\d+)</rpc_seqno>", first)
    again = first.replace(m.group(0), "<rpc_seqno>%d</rpc_seqno>" % (int(m.group(1)) + 1))
    again = again.replace("<hostid>0</hostid>", "<hostid>1</hostid>")
    sc.add("sched", "sched-too-soon", sanitize_sched(again).encode(), "request.xml", gap=0,
           derived="the first request again, from host 1 with rpc_seqno one higher, sent at once: "
           "a client that asks for work again before min_sendwork_interval has passed")
    listed = again.replace("<rpc_seqno>%d</rpc_seqno>" % (int(m.group(1)) + 1),
                           "<rpc_seqno>%d</rpc_seqno>" % (int(m.group(1)) + 2))
    listed = listed.replace("<other_results>\n</other_results>",
                            "<other_results>\n    <other_result>\n        <name>diff_0001_0</name>\n"
                            "        <app_version>100</app_version>\n    </other_result>\n</other_results>")
    assert "<other_result>" in listed
    sc.add("sched", "sched-too-soon-listed", sanitize_sched(listed).encode(), "request.xml", gap=0,
           derived="the request before with its task listed under other_results, so that nothing is "
           "lost and the server comes to the check of min_sendwork_interval")
    forgot = again.replace("<rpc_seqno>%d</rpc_seqno>" % (int(m.group(1)) + 1),
                           "<rpc_seqno>%d</rpc_seqno>" % (int(m.group(1)) + 3))
    sc.add("sched", "sched-forgot-jobs-left", sanitize_sched(forgot).encode(), "request.xml",
           derived="the second request again, after the wait, with three jobs still unsent: the "
           "lost task is resent, and upstream sends nothing more")
    sc.finish()


def build_web():
    sc = Scenario("web")
    first = requests("work", "sched")[0][2]
    sc.add("sched", "sched-first", sanitize_sched(first.decode()).encode(), "request.xml")
    q = urllib.parse.quote
    hand = "written by hand: the request an account manager or a browser sends"
    for label, url in [
        ("get_project_config", "get_project_config.php"),
        ("server_status", "server_status.php?xml=1"),
        ("show_user-id", "show_user.php?format=xml&userid=1"),
        ("show_user-auth", "show_user.php?format=xml&auth=" + AUTH),
        ("show_user-unknown", "show_user.php?format=xml&userid=999"),
        ("am_get_info", "am_get_info.php?account_key=" + AUTH),
        ("am_get_info-bad-key", "am_get_info.php?account_key=" + "0" * 32),
        ("lookup_account-no-hash", "lookup_account.php?email_addr=tester%40example.org&passwd_hash="),
        ("am_set_info", "am_set_info.php?account_key=%s&name=%s&venue=work&send_email=0" % (AUTH, q("Renamed User"))),
    ]:
        sc.add("web", label, method="GET", url=url, derived=hand)
    sc.finish()


os.makedirs(args.out, exist_ok=True)
wanted = args.scenarios.split()
for s in ["attach", "work", "report", "anon"]:
    if s in wanted:
        build_captured(s)
for name, fn in [("lost", build_lost), ("quick", build_quick), ("uploads", build_uploads),
                 ("web", build_web)]:
    if name in wanted:
        fn()
print("corpus written to", args.out)
