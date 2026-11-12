# Client request corpus

Scheduler requests recorded from stock BOINC clients of six versions, scrubbed, and replayed by
`cloud/crates/cb-core/tests/client_corpus.rs`. They show what each client version sends, and
that this server's request parser and `handle_request` accept it.

`manifest.json` lists every file with its client version and scenario, and says where each
client binary came from. The directory of a version holds one file per scenario:

| File | Scenario | What the client was doing |
|------|----------|---------------------------|
| `01-first-contact` | `first_contact` | Attached, no host ID yet (`<hostid>0</hostid>`), asks for one second of work |
| `02-work-request-task-held` | `work_request` | Asks for work while it holds a task (`<other_results>`) |
| `03-report-one-task-two-held` | `report` | Reports one finished task and still holds two |
| `04-report-two-tasks` | `report` | Reports the other two |
| `05-no-work-nothing-to-report` | `no_work` | A request with nothing to report, for work the project does not have |
| `06-work-request-after-restart` | `work_request` | After a restart while a task was running |
| `07-report-after-restart` | `report` | Reports the task that was running at the restart |
| `08-reattach-first-contact` | `reattach` | After detach and attach in the same data directory: host ID 0 again, same host CPID (the scrubbed files carry a placeholder) |
| `09-reattach-work-request-task-held` | `work_request` | The next request after the re-attach |

The versions are 7.16.6, 7.18.1, 7.20.5, 7.24.1, 8.0.4 and 8.2.15, all `x86_64-pc-linux-gnu`.
Windows, macOS and Android clients, and GPU hosts, are not in the corpus.

## What the tests check

- Each file parses, with the client version the manifest gives and the platform
  `x86_64-pc-linux-gnu`.
- `unread_elements_are_the_documented_ones` finds, for every value in every file, whether
  changing it changes what the parser returns. A value that changes nothing is an element the
  server ignores. The list is in the test (`UNREAD`), with what the stock parser in
  `sched/sched_types.cpp` does with each. A client that sends a new element makes the test fail
  until it is added there.
- `every_fixture_replays_through_handle_request` builds a SQLite world for each file: the host
  the client already had, the tasks it reports or holds (made in progress for that host), and
  unsent jobs when the scenario is one where the project has work. It runs `handle_request`
  and checks that the reply is well formed, that every reported task is acknowledged and
  recorded as over with a success outcome, that a first contact gets a host ID, that a
  re-attach finds the existing host and does not make a second, and that work is sent when
  there is some and refused with "Project has no tasks available" when there is none.
- `fixtures_are_scrubbed` runs the checks of the scrub script over the committed files.

The recorded host ID (100 for 7.16.6, 105 for 8.2.15) is not the ID the test world gives its host,
so the test replaces a non-zero `<hostid>` before replaying. The request's code-signing key is a placeholder, and the test world
has no code-signing key, so what the server does when a client holds another key is not covered.

## Capturing a request

1. Start a client in a new, empty data directory against a project you may test on, with its own
   GUI RPC port and `--allow_multiple_clients`. Do not use a data directory or a client that is
   attached to another project.
2. Put these log flags in the data directory's `cc_config.xml` before the first start:
   `<sched_op_debug>`, `<sched_ops>`, `<task>`, `<file_xfer>` and `<unparsed_xml>`. The client's
   message log then shows each scheduler request and any element of a reply it did not read.
   Leave `<http_debug>` off unless you need it: it writes request bodies, with the account key,
   into the log.
3. The client writes each request to `sched_request_<project>.xml` in its data directory and
   overwrites it with the next. Copy the file after each RPC (a loop that compares the
   modification time every 0.1 s is enough; copy a reply only when it is not empty, because the
   client truncates the file when it starts the next request).
4. `boinccmd --project URL update` makes the client send a request now. Detaching and attaching
   again, quitting and restarting, and submitting jobs between steps give the other scenarios.

## Scrubbing

```
cloud/dev/scrub-sched-request.py --input RAW.xml --output cloud/tests/client-corpus/VERSION/NN-name.request.xml \
    --forbid NAME-OF-THE-CAPTURE-HOST
```

The script replaces the account key, the cross-project ID, the host CPID, the email hash, the
domain name, the address, the CPU model and features, the kernel release, product and serial
strings, MAC address, output file checksums, paths, the project's URLs and the echoed code-signing
key. It leaves element names, numbers, counts, task names and timing as they were. It then refuses
to write the file if the result still has a run of 32 or more hex digits, an IPv4 address outside
the RFC 5737 ranges, or a word given with `--forbid`. Give it the capture machine's host name, and
the name of any user or machine that is not in its list, with `--forbid` (it can be repeated).

To check files by hand:

```
for f in cloud/tests/client-corpus/*/*.xml; do cloud/dev/scrub-sched-request.py --check-only --input "$f"; done
grep -rEn '[0-9a-fA-F]{32,}' cloud/tests/client-corpus/*/
grep -rEoh '([0-9]{1,3}\.){3}[0-9]{1,3}' cloud/tests/client-corpus/*/ | sort | uniq -c
```

The first `grep` prints nothing. The second prints only `192.0.2.1`, an address from RFC 5737,
with a count.

A new fixture goes in `manifest.json` under its client version with a scenario from the table,
and `cargo test -p cb-core --test client_corpus` must pass.
