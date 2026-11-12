# Client request corpus

Scheduler requests recorded from stock BOINC clients of six versions on 64-bit Linux, and of
clients on six other platforms, scrubbed, and replayed by
`cloud/crates/cb-core/tests/client_corpus.rs`. They show what each client version and platform
sends, and that this server's request parser and `handle_request` accept it.

`manifest.json` lists every file with its client version, platform and scenario, and says where
each client binary came from. A client that is not 64-bit Linux has `platform` set to the name it
sends in `<platform_name>`. The directory of a client holds one file per scenario:

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
| `10-report-after-resumed-upload` | `report` | Only in `8.2.15-windows_x86_64`: reports a task whose 20 MB output upload was cut and resumed (`final_peak_*` elements, `max_nbytes` 30000000) |

The directories named by a version alone are the 64-bit Linux clients: 7.16.6, 7.18.1, 7.20.5,
7.24.1, 8.0.4 and 8.2.15, all `x86_64-pc-linux-gnu`. The others are named by version and platform:

| Directory | Client | Platform |
|-----------|--------|----------|
| `8.0.4-i686-pc-linux-gnu` | Debian package for i386 | `i686-pc-linux-gnu` |
| `8.0.4-aarch64-unknown-linux-gnu` | Debian package for arm64, under qemu-user | `aarch64-unknown-linux-gnu` |
| `8.0.4-arm-unknown-linux-gnueabihf` | Debian package for armhf, under qemu-user | `arm-unknown-linux-gnueabihf` |
| `8.2.11-arm64-apple-darwin` | macOS client on an Apple-silicon Mac | `arm64-apple-darwin` |
| `8.2.15-windows_x86_64-wine` | the Windows client binary under Wine, not a Windows host | `windows_x86_64` |
| `8.2.15-windows_x86_64` | the stock Windows client, installed with its installer on a Windows 11 virtual machine | `windows_x86_64` |
| `8.0.4-i386-on-64-bit-kernel` | the i386 package on a 64-bit kernel, for an alternate platform | `x86_64-pc-linux-gnu`, alternate `i686-pc-linux-gnu` |

In `8.2.15-windows_x86_64`, file `03` reports one task with four held, and file `04` reports four tasks, where the other clients' files have two held and two reported: two jobs of earlier runs were sent to that host again.

Android clients, 32-bit Windows and macOS clients, and GPU hosts other than the Apple GPU of the
macOS client are not in the corpus. The last directory has first-contact, work-request, report and no-work files only, and
`manifest.json` says so with `scenarios`; its `version_platform` is the platform of the app version
that the replay serves it.

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
- `fixtures_are_scrubbed` runs the checks of the scrub script over the committed files, among them
  that no MAC address (six hex pairs joined by colons or dashes) is left under any element name.
  `scrub_script_removes_and_refuses_mac_addresses` runs the script itself on a made-up request.

The recorded host ID (100 for 7.16.6, 105 for 8.2.15) is not the ID the test world gives its host,
so the test replaces a non-zero `<hostid>` before replaying. The request's code-signing key is a placeholder, and the test world
has no code-signing key, so what the server does when a client holds another key is not covered.

## Capturing a request

1. Start a client in a new, empty data directory against a project you may test on, with its own
   GUI RPC port and `--allow_multiple_clients`. Do not use a data directory or a client that is
   attached to another project. Clients that share a computer need different data directory paths:
   a client's host CPID is the MD5 of the computer's MAC address and the data directory path, so
   two containers on one network with the same path have the same CPID.
2. Put these log flags in the data directory's `cc_config.xml` before the first start:
   `<sched_op_debug>`, `<sched_ops>`, `<task>`, `<file_xfer>` and `<unparsed_xml>`. The client's
   message log then shows each scheduler request and any element of a reply it did not read.
   Leave `<http_debug>` off unless you need it: it writes request bodies, with the account key,
   into the log. A log written with it on stays on the machine that made it.
3. The client writes each request to `sched_request_<project>.xml` in its data directory and
   overwrites it with the next. Copy the file after each RPC (a loop that compares the
   modification time every 0.1 s is enough; copy a reply only when it is not empty, because the
   client truncates the file when it starts the next request). A copy made while the client is
   writing can be empty; keep the next one.
4. `boinccmd --project URL update` makes the client send a request now. Detaching and attaching
   again, quitting and restarting, and submitting jobs between steps give the other scenarios.
   Submit the job for the step after a re-attach only once the client has attached: the client
   sends a last request while it detaches, the server can send the job in answer to it, and the
   client drops the reply.
5. Give the run one app with a version for each platform, and run one client at a time. A second
   client would take the jobs meant for the first.

Other platforms:

- 32-bit Linux and ARM Linux: a stock distribution package installed with `apt` in a container of
  the platform (`podman run --platform linux/386`, `linux/arm64`, `linux/arm/v7`). A 32-bit
  client on a 64-bit kernel names `x86_64-pc-linux-gnu` first; `setarch i686` in front of the
  client makes `uname -m` say i686, as a 32-bit kernel does. ARM binaries run on an x86-64 machine
  under qemu-user, registered in `binfmt_misc` with the fix-binary flag (`mount -t binfmt_misc`
  inside a container that has none, and the `qemu-aarch64` and `qemu-arm` binaries from the
  `qemu-user` package, extracted, not installed). What the client reads from `/proc/cpuinfo` is
  qemu's.
- macOS: the installed client binary copied to a scratch directory and started as a second
  instance, so that the installed one is not touched: `--dir`, `--allow_multiple_clients`,
  `--gui_rpc_port`, and `-insecure` because the data directory is not owned by the accounts of the installed client (the client says "File ownership or permissions are set in a way that does not allow
  sandboxed execution" and exits without it). Run from inside the application bundle,
  the client could not create files in a data directory under `/tmp` ("Can't open
  client_state_next.xml: fopen() failed"); the same binary copied to a scratch directory could. There is no `boinccmd` in the bundle; the
  GUI RPC is a TCP socket with an MD5 password handshake. `<ncpus>` in `cc_config.xml` changes the
  `p_ncpus` the client reports; `max_ncpus_pct` in `global_prefs_override.xml` limits the CPUs
  without that, and a value below one CPU's share means no CPU at all. The client runs `podman
  machine init` and `start` at startup if `podman` is installed, even with `<dont_use_docker>`.
- Windows: the client from the official installer, taken out with 7z (the installer is a WiX
  bundle holding an MSI, whose `boinc.cab` holds `boinc.exe` and `boinccmd.exe`), run with Wine in
  an amd64 container: `wine boinc.exe --dir Z:/work/wine ...`, `boinccmd.exe` through Wine too.
  Label every file "Windows client binary under Wine, not a Windows host".
- A Windows host: the stock installer (`boinc_8.2.15_windows_x86_64.exe`, from
  `boinc.berkeley.edu/dl/`) with `/quiet`, on a Windows 11 Enterprise evaluation virtual machine
  (Microsoft's evaluation image, installed unattended with an answer file). The installer's silent mode
  starts no client. Put the log flags in `C:\ProgramData\BOINC\cc_config.xml`, start
  `boinc.exe --redirectio --launched_by_manager` in that directory as the logged-in user (a
  scheduled task with `/IT` does it from a remote session), and run `boinccmd.exe` with the password
  in `gui_rpc_auth.cfg`. A PowerShell loop that copies `sched_request_*.xml` and `sched_reply_*.xml` each
  time their modification time changes does the capture (open them with `FileShare.ReadWrite`). Windows Update
  runs for a long time on a new virtual machine and slows everything; stop `wuauserv` and `TrustedInstaller`
  once the system is installed. To cut an upload, `Disable-NetAdapter` and `Enable-NetAdapter` a
  few seconds apart, with `<max_bytes_sec_up>` in `global_prefs_override.xml` low enough that the
  upload lasts a minute; a file of 8 MiB or less restarts from offset 0 (D5).

## Scrubbing

```
cloud/dev/scrub-sched-request.py --input RAW.xml --output cloud/tests/client-corpus/VERSION/NN-name.request.xml \
    --forbid NAME-OF-THE-CAPTURE-HOST
```

The script replaces the account key, the cross-project ID, the host CPID, the email hash, the
domain name, the address, the time zone, the CPU model and features (by a placeholder of the kind
of the request's platform: x86, 64-bit ARM, 32-bit ARM, Apple silicon), the kernel release, product and serial
strings, absolute Windows paths (`C:\Users\...`, `C:\ProgramData\...`), the MAC address (`<mac_address>`), GPU and OpenCL device names and PCI numbers, WSL
distribution names, user and team names, the client brand, output file checksums, paths, the
project's URLs and the echoed code-signing key. It leaves element names, numbers, counts, task
names and timing as they were. It then refuses to write the file if the result still has a run of
32 or more hex digits, a MAC address (six hex pairs joined by colons or dashes) in any element, an
IPv4 address outside the RFC 5737 ranges, a `\Users\` or `/Users/` path, or a word given with `--forbid`. Give it the capture
machine's host name, and the name of any user or machine that is not in its list, with `--forbid`
(it can be repeated).

To check files by hand (a file with `<mac_address>02:00:5e:00:53:01</mac_address>` in it fails
the first command):

```
for f in cloud/tests/client-corpus/*/*.xml; do cloud/dev/scrub-sched-request.py --check-only --input "$f"; done
grep -rEn '[0-9a-fA-F]{32,}' cloud/tests/client-corpus/*/
grep -rEoh '([0-9]{1,3}\.){3}[0-9]{1,3}' cloud/tests/client-corpus/*/ | sort | uniq -c
```

The first `grep` prints nothing. The second prints only `192.0.2.1`, an address from RFC 5737,
with a count.

A new fixture goes in `manifest.json` under its client version with a scenario from the table,
and `cargo test -p cb-core --test client_corpus` must pass.
