# subagent-net

Distributed network of resumable LLM agents, written in Rust. The architecture, event model, pause semantics and milestones are in [DESIGN.md](DESIGN.md). Read it before changing `core` or the hub/spawner protocol.

- `core` stays free of I/O: `step()` must remain pure and deterministic.
- Change the Postgres schema only through `sqlx migrate`.
- Toolchain comes from the nix devshell (`nix develop`, or direnv): rust-overlay stable, Postgres, sqlx-cli.
