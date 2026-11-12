#!/usr/bin/env python3
"""Copies a stock project's rows into SQL for a cloud-boinc database.

This is the smallest copy the migration check needs, not an importer (issue
#17 is the importer). It runs inside the stock server's container, where the
MariaDB socket and `MySQLdb` are, and writes one file of INSERT statements.
For each table it carries the columns that exist in both schemas, which it
learns from the stock database and from cloud/migrations/*.sql applied to an
in-memory SQLite database; columns only the stock table has are dropped and
columns only cloud-boinc has take their defaults. What it carried and what it
made up is printed, one line per table, with the counts.

    copy_rows.py --db NAME --socket PATH --migrations DIR --project-dir DIR
                 --out FILE [--pepper-file FILE] [--wu-prefix PREFIX]... [--skip-host ID]...

Tables: platform, app, app_version, user, host and host_app_version always;
workunit and result only for the workunits whose name starts with a --wu-prefix
(compared as text, not as a LIKE pattern, so `_` and `%` mean themselves; give it more than
once for several), with every result of
those workunits. Without the option no job rows are copied. A host given with
--skip-host (more than once for several) is left out of host and host_app_version, to
see what happens to a computer whose row did not come across. The conversions:

  user.passwd_hash   Stock stores PHP password_hash() of md5(password +
                     lowercase email), or, in the old form, that md5 itself.
                     A value that is a bare 32-digit md5 is replaced by
                     HMAC-SHA-256(pepper, md5) when --pepper-file is given,
                     which is how cloud-boinc stores it. Any other value (a
                     bcrypt hash) is copied as it is: it cannot be converted,
                     and cloud-boinc does not accept it as a password.
  workunit.result_template
                     Stock keeps the name of a template file
                     (result_template_file); cloud-boinc keeps the text. It is
                     read from the project directory.
  *.mod_time         A TIMESTAMP in the stock schema, seconds since 1970 in this one; read
                     as UTC.
  user.email_addr    Copied as it is (see the README: a stock address with
                     capitals is a finding).
"""
import argparse
import calendar
import datetime
import hashlib
import hmac
import os
import re
import sqlite3
import sys

import MySQLdb

ap = argparse.ArgumentParser()
ap.add_argument("--db", required=True)
ap.add_argument("--socket", required=True)
ap.add_argument("--migrations", required=True)
ap.add_argument("--project-dir", required=True)
ap.add_argument("--out", required=True)
ap.add_argument("--pepper-file")
ap.add_argument("--wu-prefix", action="append", default=[])
ap.add_argument("--skip-host", action="append", default=[], type=int)
args = ap.parse_args()

pepper = None
if args.pepper_file:
    pepper = open(args.pepper_file).read().strip().encode()

# cloud-boinc's columns, from its own migrations.
lite = sqlite3.connect(":memory:")
for name in sorted(os.listdir(args.migrations)):
    if name.endswith(".sql"):
        lite.executescript(open(os.path.join(args.migrations, name)).read())


def ours(table):
    return [r[1] for r in lite.execute("PRAGMA table_info(%s)" % table)]


db = MySQLdb.connect(db=args.db, unix_socket=args.socket, user="boincadm", charset="utf8mb4")
cur = db.cursor()


def theirs(table):
    cur.execute("SHOW COLUMNS FROM `%s`" % table)
    return [r[0] for r in cur.fetchall()]


def lit(v):
    if v is None:
        return "NULL"
    if isinstance(v, datetime.datetime):
        # workunit.mod_time and result.mod_time are TIMESTAMP columns in the stock schema
        # and seconds since 1970 in this one; the server's time zone here is UTC.
        return str(calendar.timegm(v.timetuple()))
    if isinstance(v, bytes):
        v = v.decode("utf-8", "replace")
    if isinstance(v, str):
        # Hex, so that a newline or a semicolon in an XML document cannot
        # confuse whatever splits the file into statements.
        return "CAST(X'%s' AS TEXT)" % v.encode().hex()
    if isinstance(v, float):
        return repr(v)
    return str(v)


TABLES = ["platform", "app", "app_version", "user", "host", "host_app_version"]
if args.wu_prefix:
    TABLES += ["workunit", "result"]
wu_where = " OR ".join("BINARY LEFT(name, %d) = BINARY %%s" % len(p) for p in args.wu_prefix)

out = open(args.out, "w")
report = []
for table in TABLES:
    mine, stock = ours(table), theirs(table)
    cols = [c for c in mine if c in stock]
    made_up = [c for c in mine if c not in stock]
    dropped = [c for c in stock if c not in mine]
    # result_template is made from the template file, not copied.
    if table == "workunit":
        made_up = [c for c in made_up if c != "result_template"]
    select = "SELECT %s FROM `%s`" % (", ".join("`%s`" % c for c in cols), table)
    if table == "workunit":
        cur.execute(select + " WHERE " + wu_where, args.wu_prefix)
    elif table == "result":
        cur.execute(select + " WHERE workunitid IN (SELECT id FROM workunit WHERE %s)" % wu_where,
                    args.wu_prefix)
    elif table == "host" and args.skip_host:
        cur.execute(select + " WHERE id NOT IN (%s)" % ",".join(["%s"] * len(args.skip_host)),
                    args.skip_host)
    elif table == "host_app_version" and args.skip_host:
        cur.execute(select + " WHERE host_id NOT IN (%s)" % ",".join(["%s"] * len(args.skip_host)),
                    args.skip_host)
    else:
        cur.execute(select)
    n = 0
    for row in cur.fetchall():
        rec = dict(zip(cols, row))
        if table == "user":
            h = rec.get("passwd_hash") or ""
            if pepper and re.fullmatch(r"[0-9a-f]{32}", h):
                rec["passwd_hash"] = hmac.new(pepper, h.encode(), hashlib.sha256).hexdigest()
        names = list(rec)
        vals = [lit(rec[c]) for c in names]
        if table == "workunit":
            cur.execute("SELECT result_template_file FROM workunit WHERE id = %s", (rec["id"],))
            tf = cur.fetchone()[0]
            text = open(os.path.join(args.project_dir, tf)).read()
            names.append("result_template")
            vals.append(lit(text))
        out.write("INSERT INTO %s (%s) VALUES (%s);\n" % (table, ", ".join(names), ", ".join(vals)))
        n += 1
    report.append("%s: %d rows; carried %d columns; made up (defaults): %s; dropped: %s" % (
        table, n, len(cols), ", ".join(made_up) or "none",
        ", ".join(dropped) or "none"))
    if table == "workunit":
        report[-1] += "; result_template read from result_template_file"
out.close()
print("\n".join(report))
