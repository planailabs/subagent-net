# subagent-net

A distributed network of resumable LLM agents, in Rust, with senses (sensors, streams, webhooks, timers, files) wired to agents through a switchboard. Every agent is an event-sourced state machine. You can pause it three ways, resume it, fork it and message it, and agents can spawn and control their own sub-agents. Any OpenAI-compatible model works; tools come from MCP servers. The hub is itself an MCP server, so other agents (Claude Code, …) can drive the network.

See [DESIGN.md](DESIGN.md) for how it works.

## Quick start

```sh
nix develop                      # rust, postgres, sqlx-cli
initdb -D .pg && pg_ctl -D .pg -l .pg/log start
createdb subnet

(cd webui && npm install && npm run build)   # the web UI, embedded into the hub

# hub + every node of the cluster file in one process; UI at http://127.0.0.1:7700
DATABASE_URL=postgres:///subnet OPENAI_API_KEY=… subnet dev examples/dev.hcl

subnet list-types
subnet spawn lead "Plan a CLI todo app and have workers draft each module" --wait
subnet list-agents
subnet tail                      # live tokens and events
subnet pause <id> --mode hard    # or safe / quick; --tree for descendants
subnet resume <id>
subnet tui                       # the agent park in the terminal
subnet watch <agent-id>          # one agent's transcript, live
```

Open http://127.0.0.1:7700 for the web UI: the agent park, live transcripts with pause/resume/approve, senses, the switchboard's routes and deliveries, and the cluster spec.

Every `subnet` command reads `./.env` and `--env-file PATH` before anything else (the real environment wins), so `DATABASE_URL`, `SUBNET_TOKEN` or API keys can live there.

A real deployment:

1. Run `subnet hub` (with `DATABASE_URL` and `SUBNET_ADMIN_TOKEN`); several hubs can share the database.
2. Describe the cluster in HCL (principals, nodes, agent types, MCP servers, mixtures, residents, senses, routes; see [examples/cluster.hcl](examples/cluster.hcl)) and `subnet apply` it.
3. Issue tokens: `subnet issue-token node gpu-1`, `subnet issue-token client claude`, …
4. On every machine: `subnet node --name gpu-1` with `SUBNET_HUB` and `SUBNET_TOKEN`. The node pulls its part of the cluster; secrets (API keys) are read from its own environment.

## Example: a virtual personality

[personality-example/](personality-example/) runs Vesper, a resident agent with a body in a 3D room. People talk to her by chat or microphone, and she answers aloud, walks around, makes coffee and remembers people. It exercises senses (webhooks, an stt stage, a timer), routes, residents, and MCP servers over stdio and HTTP. `cargo run -p personality -- up`; see its [README](personality-example/README.md).

## Driving it from another agent

Add the hub as an HTTP MCP server, e.g. for Claude Code:

```sh
claude mcp add --transport http subnet http://127.0.0.1:7700/mcp \
  --header "Authorization: Bearer $CLAUDE_TOKEN"   # from: subnet issue-token client claude
```

The caller then gets `spawn`, `send`, `wait_inbox`, `pause`, `resume`, `approve`, `fork`, `transcript`, `list_agents` and `list_types`. The same operations are a REST/RPC API (`/v1/…`, OpenAPI at `/v1/openapi.json`, docs at `/v1/docs`) and `subnet` subcommands.

## Tests

```sh
nix develop -c cargo test
```

Web UI logic: `cd webui && npm test`.

The tests start a throwaway Postgres under `target/tmp/testpg` (stop it with `pg_ctl -D target/tmp/testpg stop`), or use `$DATABASE_URL`. They cover:

- the state machine: every pause mode, crash recovery, approval, children, budgets, parallel tools
- the LLM client against a mock SSE server; external executors
- the hub against real Postgres: placement, fencing, dormancy, snapshots, forks, auth, HA elections
- cluster files, including the example in DESIGN.md
- MCP servers (stdio and HTTP, local and routed through the hub) with cancellation
- senses, streams across nodes, blobs, and the switchboard's routes and deliveries
- the API (REST/RPC, OpenAPI, MCP, SSE), the CLI binary, the web UI's serving and sign-in, the TUI
- `kill -9` of node and hub processes mid-stream, including a leader hub handing over to a standby
