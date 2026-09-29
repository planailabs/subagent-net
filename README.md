# subagent-net

A distributed network of resumable LLM agents, in Rust. Every agent is an event-sourced state machine. You can pause it three ways, resume it, fork it and message it, and agents can spawn and control their own sub-agents. Any OpenAI-compatible model works; tools come from MCP servers. The hub is itself an MCP server, so other agents (Claude Code, …) can drive the network.

See [DESIGN.md](DESIGN.md) for how it works.

## Quick start

```sh
nix develop                      # rust, postgres, sqlx-cli
initdb -D .pg && pg_ctl -D .pg -l .pg/log start
createdb subnet

# hub + every node of the cluster file in one process
DATABASE_URL=postgres:///subnet OPENAI_API_KEY=… subnet dev examples/dev.hcl

subnet list-types
subnet spawn lead "Plan a CLI todo app and have workers draft each module" --wait
subnet list-agents
subnet tail                      # live tokens and events
subnet pause <id> --mode hard    # or safe / quick; --tree for descendants
subnet resume <id>
```

A real deployment:

1. Run `subnet hub` (with `DATABASE_URL` and `SUBNET_ADMIN_TOKEN`); several hubs can share the database.
2. Describe the cluster in HCL (principals, nodes, agent types, MCP servers, mixtures, residents, senses, routes; see [examples/cluster.hcl](examples/cluster.hcl)) and `subnet apply` it.
3. Issue tokens: `subnet issue-token node gpu-1`, `subnet issue-token client claude`, …
4. On every machine: `subnet node --name gpu-1` with `SUBNET_HUB` and `SUBNET_TOKEN`. The node pulls its part of the cluster; secrets (API keys) are read from its own environment.

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

The tests start a throwaway Postgres under `target/tmp/testpg`, or use `$DATABASE_URL`. They cover:

- the state machine: every pause mode, crash recovery, approval, children, budgets
- the LLM client against a mock SSE server
- the hub against real Postgres
- MCP tools, over stdio and streamable HTTP, including cancellation
- the user surfaces and the CLI binary
- `kill -9` failover of spawner and hub processes mid-stream
