# cloud-boinc

A BOINC project server for Cloudflare Workers, written in Rust. Stock BOINC clients talk to it
with the same protocols they use against the C++ server in this repository.

Status: a stock 8.0.4 client attaches, downloads a signed app, runs jobs, uploads and reports
against the dev deployment at `boinc-dev.usnewsmap.com`. Some server features are not
implemented; see the list in [docs/architecture.md](docs/architecture.md#implementation-status).

- [docs/architecture.md](docs/architecture.md): design, cost, measured performance
- [docs/deploy.md](docs/deploy.md): local stack and deployment
