#!/usr/bin/env python3
"""Loads the starting state of fixtures/project.json into a stock project.

Runs once, as the project's user, after tools/make_project. It uses the
project's own tools wherever there is one: xadd for platforms and apps,
update_versions for app versions, stage_file_native and create_work for jobs, and one pass of the
transitioner, which makes the results of the new workunits.
Users are inserted with SQL, since the stock server has no tool that takes a
given authenticator.
"""
import argparse
import glob
import json
import os
import shutil
import subprocess
import sys
import time

import MySQLdb

ap = argparse.ArgumentParser()
ap.add_argument("--project", required=True)
ap.add_argument("--fixtures", required=True)
ap.add_argument("--socket", required=True)
ap.add_argument("--db", required=True)
args = ap.parse_args()
proj, fx = args.project, args.fixtures
spec = json.load(open(os.path.join(fx, "project.json")))


def run(*cmd):
    print("+", " ".join(cmd), flush=True)
    subprocess.run(cmd, cwd=proj, check=True)


# Keys: the same pairs the native side signs with.
for name in glob.glob(os.path.join(fx, "keys", "*")):
    shutil.copy(name, os.path.join(proj, "keys", os.path.basename(name)))

# Platforms and apps, through project.xml and xadd.
xml = ["<boinc>"]
for p in spec["platforms"]:
    xml.append("<platform><name>%s</name><user_friendly_name>%s</user_friendly_name></platform>"
               % (p["name"], p["user_friendly_name"]))
for a in spec["apps"]:
    xml.append("<app><name>%s</name><user_friendly_name>%s</user_friendly_name></app>"
               % (a["name"], a["user_friendly_name"]))
xml.append("</boinc>")
open(os.path.join(proj, "project.xml"), "w").write("\n".join(xml) + "\n")
run("bin/xadd")

# Settings: the options of config-options.xml go into config.xml.
cfg_path = os.path.join(proj, "config.xml")
cfg = open(cfg_path).read()
opts = open(os.path.join(fx, "config-options.xml")).read()
cfg = cfg.replace("</config>", opts + "</config>", 1)
open(cfg_path, "w").write(cfg)

# Templates.
for t in glob.glob(os.path.join(fx, "templates", "*")):
    shutil.copy(t, os.path.join(proj, "templates", os.path.basename(t)))

# App versions: apps/<app>/<version>/<platform>/<files>, then update_versions.
for v in spec["app_versions"]:
    n = v["version_num"]
    d = os.path.join(proj, "apps", v["app"], "%d.%02d" % (n // 100, n % 100), v["platform"])
    os.makedirs(d, exist_ok=True)
    for f in v["files"]:
        shutil.copy(os.path.join(fx, "app", f["name"]), os.path.join(d, f["name"]))
run("bin/update_versions", "--noconfirm")

def stored_passwd_hash(md5):
    """What the site stores: password_hash() of the client's md5 hash.

    The old form, the md5 itself, reaches a branch of check_passwd_hash() in
    html/inc/user_util.inc that calls hash_equal(), a function PHP does not
    have, so lookup_account.php dies on it.
    """
    out = subprocess.run(["php", "-r", "echo password_hash($argv[1], PASSWORD_DEFAULT);", md5],
                         check=True, capture_output=True, text=True)
    return out.stdout


# Users.
db = MySQLdb.connect(db=args.db, unix_socket=args.socket, user="boincadm")
cur = db.cursor()
for u in spec["users"]:
    cur.execute(
        "INSERT INTO user (id, create_time, email_addr, name, authenticator, country, "
        "postal_code, total_credit, expavg_credit, expavg_time, global_prefs, project_prefs, "
        "teamid, venue, url, send_email, show_hosts, posts, seti_id, seti_nresults, "
        "seti_last_result_time, seti_total_cpu, signature, has_profile, cross_project_id, "
        "passwd_hash, email_validated, donated) "
        "VALUES (%s, %s, %s, %s, %s, %s, %s, 0, 0, %s, %s, %s, 0, '', '', 1, 1, 0, 0, 0, 0, 0, "
        "'', 0, %s, %s, 0, 0)",
        (u["id"], u["create_time"], u["email_addr"], u["name"], u["authenticator"],
         u["country"], u["postal_code"], u["create_time"], u["global_prefs"],
         u["project_prefs"], u["cross_project_id"], stored_passwd_hash(u["passwd_md5"])))
db.commit()

# Jobs: stage the input file, then create the workunit and its result.
for j in spec["jobs"]:
    run("bin/stage_file_native", "--copy", os.path.join(fx, "inputs", j["input"]))
    run("bin/create_work", "--appname", j["app"], "--wu_name", j["name"],
        "--wu_template", "templates/%s_in" % j["app"],
        "--result_template", "templates/%s_out" % j["app"], j["input"])
# The transitioner makes the results of new workunits; it takes only those
# whose transition time is in the past.
time.sleep(2)
run("bin/transitioner", "-d", "2", "--one_pass")
print("fixture loaded")
