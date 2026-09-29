# subagent-net

A distributed network of resumable LLM agents, in Rust. Every agent is an event-sourced state machine. You can pause it three ways, resume it, fork it and message it, and agents can spawn and control their own sub-agents. Any OpenAI-compatible model works; tools come from MCP servers. The hub is itself an MCP server, so other agents (Claude Code, …) can drive the network.

See [DESIGN.md](DESIGN.md) for how it works.

## Quick start

```sh
nix develop                      # rust, postgres, sqlx-cli
initdb -D .pg && pg_ctl -D .pg -l .pg/log start
createdb subnet

# hub + one spawner in one process
DATABASE_URL=postgres:///subnet OPENAI_API_KEY=… subnet dev -c examples/spawner.toml

subnet types
subnet spawn lead "Plan a CLI todo app and have workers draft each module" --wait
subnet agents
subnet tail                      # live tokens and events
subnet pause <id> --mode hard    # or safe / quick; --tree for descendants
subnet resume <id>
```

For a real deployment, run `subnet hub` once and `subnet spawner -c <config>` on every machine that has models or tools. Set `SUBNET_TOKEN` everywhere.

## Driving it from another agent

Add the hub as an HTTP MCP server, e.g. for Claude Code:

```sh
claude mcp add --transport http subnet http://127.0.0.1:7700/mcp \
  --header "Authorization: Bearer $SUBNET_TOKEN" --header "x-subnet-as: claude"
```

The caller then gets `spawn`, `send`, `wait_inbox`, `pause`, `resume`, `approve`, `fork`, `transcript`, `list_agents` and `list_types`.

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
