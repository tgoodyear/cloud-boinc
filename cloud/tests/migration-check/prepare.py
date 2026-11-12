#!/usr/bin/env python3
"""Makes the fixture directory the stock server of the migration check is
started with (see README.md).

The directory has the layout tests/differential/stock/load_fixture.py reads:
fresh code-signing and upload key pairs, two platforms (Linux and anonymous), one application with
one signed version, three accounts, and no jobs. The accounts' keys are random
and written to <out>/secrets.json, which is for the scripts of this check and
is never printed.

    prepare.py --out DIR --cboinc PATH --app-binary PATH [--project NAME]
"""
import argparse
import hashlib
import json
import os
import secrets
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
DIFF = os.path.join(HERE, "..", "differential", "project")

ap = argparse.ArgumentParser()
ap.add_argument("--out", required=True)
ap.add_argument("--cboinc", required=True, help="the cboinc binary, for key generation")
ap.add_argument("--app-binary", required=True, help="the test application (tests/app), static")
ap.add_argument("--project", default="proj")
args = ap.parse_args()

out = os.path.abspath(args.out)
os.makedirs(os.path.join(out, "keys"), exist_ok=True)
os.makedirs(os.path.join(out, "app"), exist_ok=True)
os.makedirs(os.path.join(out, "inputs"), exist_ok=True)
for name in ("code_sign", "upload"):
    priv = os.path.join(out, "keys", name + "_private")
    pub = os.path.join(out, "keys", name + "_public")
    if not os.path.exists(priv):
        subprocess.run([args.cboinc, "keygen", "--private", priv, "--public", pub], check=True)

shutil.copytree(os.path.join(DIFF, "templates"), os.path.join(out, "templates"), dirs_exist_ok=True)
# The differential test's options (upload certificates on, lost results resent), and
# a limit on the tasks one computer holds, so that each client gets the same four jobs.
with open(os.path.join(DIFF, "config-options.xml")) as f:
    options = f.read()
with open(os.path.join(out, "config-options.xml"), "w") as f:
    f.write(options)
    f.write("<max_wus_in_progress>4</max_wus_in_progress>\n<max_wus_to_send>4</max_wus_to_send>\n")
    # A reply's <request_delay> is this plus one: the time a client waits before its next
    # automatic request, long enough to leave a finished task unreported on purpose.
    f.write("<min_sendwork_interval>31</min_sendwork_interval>\n")
shutil.copy(os.path.join(DIFF, "inputs", "diff_in.txt"), os.path.join(out, "inputs", "in.txt"))
app_name = "upper_1.00_x86_64-pc-linux-gnu"
shutil.copy(args.app_binary, os.path.join(out, "app", app_name))
os.chmod(os.path.join(out, "app", app_name), 0o755)

PASSWORDS = {1: "migration-check-password-1", 2: "migration-check-password-2",
             3: "migration-check-password-3"}
users = []
secrets_out = {}
for uid, (email, name) in enumerate(
    [("migrate.one@example.org", "Migrate One"), ("migrate.two@example.org", "Migrate Two"),
     ("Migrate.Three@Example.ORG", "Migrate Three")], 1
):
    auth = secrets.token_hex(16)
    # What the client sends in lookup_account: the address in lower case.
    md5 = hashlib.md5((PASSWORDS[uid] + email.lower()).encode()).hexdigest()
    users.append({
        "id": uid, "create_time": 1700000000 + uid, "email_addr": email, "name": name,
        "authenticator": auth, "cross_project_id": secrets.token_hex(16), "passwd_md5": md5,
        "global_prefs": "", "project_prefs": "", "country": "United States", "postal_code": "",
    })
    secrets_out[str(uid)] = {"email": email, "password": PASSWORDS[uid], "authenticator": auth,
                             "passwd_md5": md5}
spec = {
    "short_name": args.project,
    "long_name": "Migration check project",
    "platforms": [
        {"name": "x86_64-pc-linux-gnu", "user_friendly_name": "Linux running on an AMD x86_64 or Intel EM64T CPU"},
        {"name": "anonymous", "user_friendly_name": "anonymous"},
    ],
    "apps": [{"name": "upper", "user_friendly_name": "Upper-case test app"}],
    "app_versions": [{"app": "upper", "platform": "x86_64-pc-linux-gnu", "version_num": 100,
                      "files": [{"name": app_name, "main_program": True}]}],
    "users": users,
    "jobs": [],
}
with open(os.path.join(out, "project.json"), "w") as f:
    json.dump(spec, f, indent=2)
# The stock server's container reads the fixtures as another user. The key pairs
# are made for this run and used nowhere else.
for name in os.listdir(os.path.join(out, "keys")):
    os.chmod(os.path.join(out, "keys", name), 0o644)
fd = os.open(os.path.join(out, "secrets.json"), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as f:
    json.dump(secrets_out, f, indent=2)
print("fixture written to", out)
