# subagent-net

Distributed network of resumable LLM agents, written in Rust (web UI: Vue + Parcel in `webui/`).

## DESIGN.md is a target, not an afterthought

[DESIGN.md](DESIGN.md) describes the architecture, protocols, file formats and behaviour. **Keep it in sync with the code:**

- A change that alters behaviour, a protocol, the cluster file format, the API or the architecture updates DESIGN.md **in the same commit**.
- When a design item is finished, move it to "implemented" in the Status section.
- If the code and DESIGN.md disagree, that is a bug. Fix whichever is wrong.

## Rules

- `core`, `cluster` and `switchboard` stay free of I/O. `Agent::apply` must remain pure and deterministic.
- Change the schema only through `sqlx migrate`, in both `crates/subnet/migrations/postgres` and `crates/subnet/migrations/sqlite` (a test checks they match).
- Every operation is defined once in the ops registry; don't hand-write MCP tools, REST handlers or CLI commands for it.
- Toolchain comes from the nix devshell (`nix develop`, or direnv): rust-overlay stable, Postgres, sqlx-cli, node.
- Tests: `nix develop -c cargo test` (starts a throwaway Postgres under `target/tmp/testpg`); `SUBNET_TEST_DB=sqlite` runs the same suite on SQLite files. Web UI: `cd webui && npm test`.
