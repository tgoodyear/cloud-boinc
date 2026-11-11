# Working in this repository

This is cloud-boinc, a fork of [BOINC/boinc](https://github.com/BOINC/boinc) that re-implements
the BOINC project server on Cloudflare Workers. The design is in
[cloud/docs/architecture.md](cloud/docs/architecture.md).

## Layout

- `cloud/` holds everything this fork adds. Work happens here.
- The rest of the tree is upstream BOINC, kept unmodified as the behaviour reference and as the
  source of the client used in end-to-end tests. The `master` branch tracks upstream; `main` is
  this fork's default branch.

## AI-assisted work and upstream

Code under `cloud/` is written with AI assistance and reviewed in this repository.

BOINC has its own
[AI usage policy](https://github.com/BOINC/boinc/blob/master/AGENTS.md) for contributions to
BOINC. It applies to anything sent to BOINC/boinc, so:

- Pull requests, issues and comments from agents go to `tgoodyear/cloud-boinc` only, with `main`
  as the base branch. Always pass `--repo tgoodyear/cloud-boinc --base main` to `gh pr create`;
  a fork's default PR target is the upstream repository.
- Agents do not open pull requests, issues or comments on BOINC/boinc. A change intended for
  upstream is prepared, verified and submitted by a person under the upstream policy.
- Do not edit files outside `cloud/`, `AGENTS.md`, `CLAUDE.md` and `.github/workflows/cloud-*.yml`
  unless the task is a merge from upstream.

## Rules for `cloud/`

- Rust, formatted with `cargo fmt`, clean under `cargo clippy --all-targets -- -D warnings`.
- Protocol and policy code lives in the plain crates (`cb-proto`, `cb-crypto`, `cb-core`) and is
  tested natively. Worker crates stay thin.
- Every SQL statement goes through the `Db` trait so it runs on both SQLite and D1.
- Scripts and CLI commands take named flags, not positional arguments.
- Documentation cites files by path and function or setting name, not by line number.
- Tests and development use only a deployment's own dev hostname, database, bucket and keys.
