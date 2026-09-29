# subagent-net design

A distributed network of LLM agents. Every agent is a resumable state machine: it can be paused, resumed, forked and messaged, and it can spawn other agents. Senses turn the outside world (sensors, microphones, webhooks, timers, files) into events, and a switchboard routes those events to agents, mailboxes and MCP tools. Users, other agents (via MCP) and a web UI drive the network.

> **This document is kept in sync with the code.** Every change to behaviour, protocol, file formats or architecture updates it in the same commit. The [Status](#status) section says what is implemented.

## Components

```
   users / MCP clients / web UI / TUI / CLI
                 │  REST+RPC API, MCP, WS/SSE events   (one ops registry)
                 ▼
 ┌──────────────────────────────────────────────┐   ┌──────────────┐
 │ hub (leader)                                 │◄──┤ hub (standby)│  leader lock in Postgres
 │  sequencer of agent logs · placement         │   └──────────────┘
 │  cluster spec (applied HCL) · principals     │
 │  switchboard: routes, mailboxes, MCP routing │
 │  blob store · stream relay                   │
 └──────┬───────────────────────────────┬───────┘
        │ WS control + WS streams       │ Postgres
        ▼                               ▼
 ┌──────────────┐ ┌──────────────┐ ┌──────────────┐
 │ node gpu-1   │ │ node laptop  │ │ node pi-hall │   each node: any mix of
 │ agents: coder│ │ agents: coder│ │ senses: door,│   agent types, MCP servers,
 │ mcp: memory  │ │ mcp: web     │ │   hall-mic   │   senses and stages
 │ senses: stt  │ │              │ │              │
 └──────────────┘ └──────────────┘ └──────────────┘
```

- **Hub**: the control plane. All state lives in Postgres; schema changes go through `sqlx migrate`. Several hubs can run against one database: one leader serves, the rest stand by (see [High availability](#high-availability)).
- **Node**: a worker process started with a hub URL, a node name and a token. It connects out to the hub (NAT-friendly), receives its part of the cluster spec, and runs the agent types, MCP servers and senses assigned to it. Credentials are resolved on the node and never leave it.
- **One binary**, `subnet`: `hub`, `node`, `dev` (hub and node in one process), `tui`, and every API operation as a CLI command.

## Cluster files (orchestration)

A cluster is described by HCL files. `subnet apply cluster.hcl` sends the files to the hub, which validates them, stores a new version and pushes each node its part. Nodes are pull-based: they only need `--hub`, `--name` and a token.

```hcl
# --- principals -------------------------------------------------------------
user "maciej"  { role = "admin" }
client "claude" { role = "operator" }
node "gpu-1"   { labels = { gpu = "a100" } }
node "laptop"  { capacity = 4 }
node "pi-hall" {}

# --- agent types ------------------------------------------------------------
agent "deepseek-flash" {
  description = "Fast, cheap generalist."
  credential {
    env      = "DEEPSEEK_KEY"                 # resolved on the node
    base_url = "https://api.deepseek.com/v1"
  }
  model         = "deepseek-chat"
  params        = { temperature = 0.3 }
  prefill       = false
  system_prompt = <<-EOT
    You are fast and terse.
  EOT
  executor { internal = true }               # or: command = ["python3", "my_agent.py"]
  nodes    = ["gpu-1", "laptop"]
  spawns   = ["deepseek-flash"]
  budget   = { max_tokens = 200000, max_depth = 2, max_children = 4 }
  approve  = ["memory.delete"]
}

# --- MCP servers ------------------------------------------------------------
mcp "memory" {
  command    = ["mcp-memory", "--db", "/var/lib/memory"]   # stdio
  env        = { LOG = "warn", TOKEN = "$MEMORY_TOKEN" }  # $VAR = node env
  nodes      = ["gpu-1"]
  idempotent = ["search"]
}
mcp "web" {
  url = "https://mcp.example.com/mcp"                     # streamable HTTP
  credential = { header = "Authorization", env = "WEB_MCP_TOKEN", prefix = "Bearer " }
  nodes = ["laptop"]
}

# --- mixtures: what can be spawned ------------------------------------------
mixture "researcher" {
  agent     = "deepseek-flash"
  mcp       = ["memory", "web"]
  mailboxes = ["door-events"]                # readable via mailbox_take
}

# --- residents: long-lived named agents --------------------------------------
resident "concierge" {
  mixture = "researcher"
  prompt  = "You watch the house. Wait for events."
}

# --- senses ------------------------------------------------------------------
sense "door" {
  node = "pi-hall"
  source { exec = ["python3", "door_i2c.py"] }             # JSON lines on stdout
  stage "debounce-bounces" { filter = "prev == null || event.state != prev.state" }
}
sense "hall-mic" {
  node = "pi-hall"
  source {
    exec   = ["arecord", "-f", "S16_LE", "-r", "16000", "-t", "raw"]
    stream = "pcm_s16le/16000"                             # binary stream, not events
  }
}
sense "hall-speech" {
  node = "gpu-1"
  source { stream = "hall-mic" }                           # hub-relayed across nodes
  stage "stt"   { exec = ["python3", "stt.py"] }           # bytes in, JSON lines out
  stage "words" { filter = "size(event.text) > 3" }
}
sense "hourly" {
  node   = "gpu-1"
  source { timer = { cron = "0 * * * *" } }
}
sense "github" {
  node   = "laptop"
  source { webhook = { path = "/github" } }                # served by the node
}

# --- switchboard ---------------------------------------------------------------
route "door-open" {
  from     = "door"
  when     = "event.state == 'open'"
  map      = "{'at': at, 'who': event.card}"
  throttle = "1/10s"
  deliver { mailbox = "door-events" }
  deliver { send = "concierge" }
}
route "speech" {
  from     = "hall-speech"
  debounce = "2s"
  deliver {
    spawn  = "researcher"
    prompt = "'Someone in the hall said: ' + event.text"
  }
  max_active = 2
}
route "log-everything" {
  from  = "door"
  batch = { window = "1m", max = 500 }
  deliver {
    mcp = { server = "memory", tool = "store", args = "{'events': batch}" }
  }
}
```

- Identity of an agent type or MCP type is `name@hash`. The hash covers everything except resolved secrets and `nodes`. An agent is only ever resumed on a node offering the same hash: changing an agent type in the cluster leaves existing agents of the old version pending (roll back, or fork them).
- A `resident` is created when its type is available, answers to whoever applied the cluster, and is cancelled when removed from the file. Changing a resident's mixture doesn't replace the running agent.
- `$VAR` in values and `env = "..."` in credentials are resolved on the node. A missing variable makes that type unavailable on that node, reported back to the hub and shown in `cluster status`.
- Durations are `250ms`, `10s`, `5m`, `2h`, `1d`. Rates are `N/duration`.
- HCL allows one attribute per line inside a block, so multi-field values on one line use object syntax: `budget = { max_tokens = 1000, max_depth = 2 }`.
- Several files can be applied together; each `kind "name"` may be declared once across them. Unknown blocks or fields are errors.
- The example above is parsed and validated by the `cluster` crate's tests, so it stays correct.
- Validation happens at apply time. Unknown references (a route to a missing mixture, a sense on an undeclared node) reject the whole version.
- `subnet apply --dry-run` prints the diff against the current version. `subnet cluster history` and `subnet cluster rollback <version>` manage versions.

## Agents

### Agent = fold over an event log

The hub is the **sequencer**. Executors *propose* events, and the hub commits them in order and echoes them back. Replicas fold only committed events. Every executor write carries the owner's **lease epoch**, and the hub rejects writes from an old epoch.

```rust
// core (crates/core/src/agent.rs): no I/O, deterministic
impl Agent {
    fn apply(&mut self, ev: &Event) -> Vec<Effect>;
    fn replay(id, spec, events) -> Agent;          // apply without effects
}

enum Event {
    Inbox { from: Addr, content, reply },
    LlmDelta { delta }, LlmDone, LlmAborted, LlmFailed { error },
    ToolResult { call_id, content, is_error }, ToolAborted { call_id },
    Approval { call_id, approved },
    ChildSpawned { id, reserved }, ChildReport { id, status, content },
    PauseRequested { mode }, Resumed, Cancelled,
    Recovered,                                     // logged by the hub on every placement
}

enum Effect { CallLlm, CallTool { call, retry }, RequestApproval { call }, AbortInflight,
              Report { to, status, content } }

enum Phase { Idle, Thinking { running }, Tools { calls }, Failed { error }, Cancelled }
// each pending tool call has its own state:
enum CallState { Queued, Running, Approval, Approved, Children { ids }, Done }
```

- The spec (mixture, agent type, MCP types, parent, budget, approval list, tool list) is stored with the agent, not in the log.
- **Parallel tools:** all tool calls of one assistant message start together (each may first need approval). The next LLM call happens once all have results. A `quick` pause lets the running calls finish but starts no queued or approved ones.
- `wait_for` is handled by the state machine itself (it waits for `ChildReport` events). Every other tool is executed by the node.
- **Recovery:** whenever the hub places an agent, it first commits `Recovered`: whatever was in flight is gone, and applying `Recovered` returns the effects that restart it. Because recovery is logged, every replica folds identical state.
- **Reports are routed by the hub** from its own replica, exactly once.
- **Snapshots:** each time an agent's log crosses a multiple of 200 events the hub stores its folded state (`agent_snapshots`, newest only). Loading an agent reads the snapshot and the events after it; `Assign` carries the snapshot and only the events after it (never a snapshot taken at the final `Recovered`, so the node always has an event to start from).
- **Forking:** `fork(id, at, tree)` copies the log prefix into a new agent. Copied history is folded without re-running its effects, so old answers aren't delivered again.
  - Without `tree`, the copy has no children: `ChildSpawned`/`ChildReport` events are left out, so a `wait_for` pending at the fork point returns "not a child".
  - With `tree`, every child spawned within the prefix is forked as well (recursively, with its full log), with ids remapped in all copied events and specs.

### Executors

- **Internal:** the node's built-in runner. It streams completions from an OpenAI-compatible endpoint, batches deltas every 250 ms, and runs tools.
- **External (`executor { command = [...] }`):** a process that replaces only the *thinking*. The node keeps running the agent's state machine, tools, approvals, pausing and failover; for every LLM call it asks the process instead. Any program can be an agent brain (another framework, a rule engine, a human-in-the-loop UI) and still gets pause, resume, fork and failover.

  The node starts one process per agent type (lazily, again after it exits) and speaks JSON lines over stdio:
  - node → process: `{"t":"hello","type","id","config"}` once, then `{"t":"think","id","agent","system","messages","tools"}` per LLM call and `{"t":"abort","id"}` on hard pause.
  - process → node: `{"t":"delta","id","delta":{"content"?, "tool_calls"?, "usage"?}}`, then `{"t":"done","id"}` or `{"t":"error","id","message"}`.

  Requests carry ids, so one process serves many agents at once. If the process exits, its open requests fail (`LlmFailed`); resuming the agent retries. `crates/subnet/examples/echo_executor.rs` is a small reference implementation.

### Pausing

A request has a mode and a scope (`tree` = the agent and all descendants). It is a logged event.

| mode    | keeps running | stops at |
|---------|---------------|----------|
| `safe`  | the whole turn | turn boundary (`Idle`) |
| `quick` | the in-flight LLM stream or tool calls; nothing new starts | next step boundary |
| `hard`  | nothing. The stream is dropped; MCP calls get `notifications/cancelled` | immediately |

**Half-responses are kept.** Streamed deltas are logged in batches, so after a hard pause or crash the partial message is whatever was committed. On resume the partial is continued: natively with `prefill = true` (vLLM `continue_final_message`), otherwise it is marked `[interrupted]` and the model is asked to go on. Half-streamed tool calls are never executed. An aborted tool call becomes `ToolAborted`; after a crash it is re-run only if it is idempotent.

### Messaging

- **Addresses:** `user:<name>`, `client:<name>`, `agent:<id>`, `resident:<name>` (resolves to the resident's agent), `mailbox:<name>`.
- A message is an `Inbox` event in the target's log: durable and ordered.
  - `Idle`: the agent wakes up.
  - `Paused`: the message is queued.
  - Otherwise: the message is injected at the next LLM call.
- **Answers:** a turn's final message goes to its askers, and to the parent as a `ChildReport`.
  - Answers to other agents are marked `reply`: they wake the receiver, but no answer is owed back, so agents can't ping-pong.
  - A turn woken only by replies or reports answers the previous askers.
  - A failed turn keeps its askers.
- **Built-in tools:**

  | tool | behaviour |
  |---|---|
  | `spawn_agent(type, prompt)` | `type` is a mixture (or bare agent type) the parent's `spawns` allows; non-blocking |
  | `send_message(to, content)` | any address |
  | `wait_for(ids)` | blocks until each child has reported |
  | `mailbox_take(name, max)` / `mailbox_peek(name, max)` | mailboxes the mixture lists |
  | `blob_get(ref)` | fetch a blob as text/base64 |
  | `list_agents`, `list_types` | |
  | `pause_agent`, `resume_agent`, `cancel_agent` | descendants only |

- **Budgets:** a child's token budget is carved out of the parent's (`ChildSpawned.reserved`). A child whose type has no limit gets half of what the parent has left. Depth shrinks per level; `max_children` is per agent.
- **Placement:** an agent needs an executor only while it has work. Dormant agents keep their slot until it's needed, and are placed again when an event gives them work. Placement picks the least-loaded live node offering the exact type hash.

## MCP servers and the MCP switchboard

- An `mcp` block declares an MCP type and the nodes that run it. On connect, a node starts its MCP servers (stdio or HTTP) and reports each type's tool list to the hub.
- A **mixture** binds an agent type to MCP types. A spawned agent's tool list is the built-ins plus the tools of its mixture's MCP types, named `<mcp>.<tool>` (e.g. `memory.store`). The list is fixed in the spec when the agent is created, so replays are stable.
- **Routing a tool call:**
  - If the agent's node runs that MCP type, the call is local.
  - Otherwise the node sends `McpCall { call_id, agent, epoch, mcp, tool, args }` to the hub. The hub checks that the agent's mixture includes that MCP type and forwards the call to the least-loaded node running it. The result comes back the same way.
  - Hard pause turns into `McpCancel`, which reaches the server as `notifications/cancelled`.
- Routes can call MCP tools directly with event data (`deliver { mcp { … } }`). The hub performs these calls like any other, with retries for idempotent tools.

## Senses, streams and the switchboard

### Events and blobs (durable)

A sense emits **events**: `{ id, sense, at, data }`. `data` is JSON. Large payloads go into the **blob store** (Postgres `blobs` table, 64 MiB limit per blob) and the event carries `"$blob": "blob:<sha256>"`. Blobs are content-addressed, fetched via the API or `blob_get`, and garbage-collected when no event, mail or agent log references them for 7 days.

### Streams (ephemeral)

A sense whose source declares `stream = "<format>"` publishes a **binary stream** named after the sense instead of events.

- A sense with `source { stream = "<name>" }` subscribes to it.
- On the same node, streams are in-process pipes. Across nodes, the hub relays them over a dedicated WebSocket (`/streams`), so audio frames never delay control traffic.
- Streams are bounded (1 MiB buffer per subscriber, drop-oldest) and never logged. Agents never see streams, only events derived from them.

### Sources and stages

- **Sources:**
  - `exec`: a command, restarted with backoff when it exits; JSON lines on stdout become events (other lines become `{"line": …}`), or raw bytes become the stream if `stream` is set.
  - `timer`: events `{"tick": n, "at": ms}`. `file`: events `{"path", "kind": create|modify|remove}` for paths matching `glob`.
  - `stream`: subscribe to another sense's stream.
  - `webhook { path }`: `POST /hooks/<path>` on the node's webhook listener (`subnet node --webhooks <addr>`, or `subnet dev --webhooks <addr>` for all in-process nodes); a JSON body is the event, anything else becomes `{"body": text}`.
  - `timer { every | cron }`
  - `file { path, glob }`: file created/changed events.
- **Stages** run in order on the sense's node (a stream subscriber's first stage must be `exec`, since it gets raw bytes):
  - `exec`: a long-running process; input on stdin (events as JSON lines, or stream bytes), output events as JSON lines. This is where STT, VAD, image models and other pre-processing live.
  - `filter` (CEL → bool) and `map` (CEL → value). CEL sees `event`, and `prev` (the previous event the stage let through; `null` at first).

### Switchboard

The switchboard runs in the hub. For every event it evaluates each `route` whose `from` matches:

CEL expressions in routes see `event` (the event data), `sense`, `at` (unix ms) and `id`; `prompt` and mcp `args` see `event` and `batch` (a list; one element unless batched). JSON integers are CEL `int` and other numbers `double`, so `event.n * 2` works. An expression that fails at runtime drops that event and counts an error; invalid expressions are rejected at apply time.

1. `when` (CEL): drop the event if false.
2. `map` (CEL): reshape the event data.
3. **Flow control**, in this order:
   - `dedupe { key, within }`: drop an event whose key was seen within the window.
   - `debounce`: deliver only the last event of a burst, after the stream has been quiet for the duration.
   - `batch { window, max }`: collect events and deliver them as one (`batch` in CEL).
   - `throttle`: at most N deliveries per interval; excess is dropped and counted.
   - `max_active`: for spawn deliveries, at most N live agents from this route. Excess waits in the route's queue (bounded at 1000, drop-oldest).
4. **Deliver** (a route may have several):
   - `spawn = "<mixture>"`, `prompt = CEL`: a new agent per delivery (per batch).
   - `send = "<resident or agent id>"`: an `Inbox` message.
   - `mailbox = "<name>"`: durable queue; agents read it with `mailbox_take`.
   - `mcp { server, tool, args = CEL }`: a direct tool call.

Every delivery is recorded (`deliveries` table: route, event, outcome), so the switchboard view can show what happened to each event. Route state (throttle buckets, debounce timers, open batches) lives in memory on the leader. After a failover, open batches and debounce windows restart empty.

## APIs: one registry, four front-ends

Every operation is defined once in the `ops` crate's typed registry: name, summary, argument and result types (serde + schemars), required role, and optionally a REST route. From the registry the hub serves:

- **REST + RPC API:** `POST /v1/ops/<name>` for every op, plus REST routes where declared, e.g.
  - `GET /v1/agents`, `GET /v1/agents/{id}`
  - `POST /v1/agents` (spawn), `POST /v1/agents/{id}/pause`
  - `GET /v1/cluster`, `PUT /v1/cluster` (apply)

  `GET /v1/openapi.json` is the generated OpenAPI 3.1 document; `/v1/docs` renders it.
- **MCP** (`/mcp`): every op a principal's role allows is a tool.
- **CLI:** every op is a `subnet` subcommand (`list_agents` → `subnet list-agents`). Path parameters are positional; an op without path parameters takes its required scalar fields positionally (`subnet spawn <type> <prompt>`). Other fields are flags, and `--json` passes a whole argument object. The CLI calls the REST/RPC API.
- **Events:** `GET /v1/events` (SSE) and `/v1/events/ws` (WebSocket), filtered by `agent` or `tree` (an agent and its descendants), later also `sense` and `route`. Each notice has a `kind`: `agent` notices carry `agent`, `ancestors`, `seq` and the committed `event`. A slow subscriber gets `{"kind":"lagged","missed":n}` instead of blocking the hub.

The web UI and the TUI use the same API.

## Auth

- Principals are declared in the cluster file: `user`, `client`, `node`. Each has a role:
  - `admin`: apply cluster files, issue tokens, everything else
  - `operator`: spawn, send, pause, resume, cancel, approve, fork
  - `viewer`: read only
  - nodes: the node protocol only
- `subnet token issue <kind> <name>` (admin) creates a random token. The hub stores only its SHA-256.
- The hub's `SUBNET_ADMIN_TOKEN` env var is a built-in `user:root` admin for bootstrap. Without it the hub runs in **open mode** (development): every caller without a known token is `user:root`.
- A principal removed from the cluster file loses access immediately; its tokens stop resolving.
- API and MCP use `Authorization: Bearer <token>`. The web UI exchanges a token for an HTTP-only session cookie (`POST /v1/login`).
- A caller's address is its principal: `user:maciej`, `client:claude`.

## High availability

- Hubs share one Postgres. Each hub tries `pg_try_advisory_lock(<cluster lock>)` on a dedicated connection; the holder is the **leader**.
- Standbys answer `503` with `x-subnet-leader: <url>` on every endpoint, and retry the lock every second.
- Nodes connect to `<hub>/node`.
- If the leader's database session dies, the lock is released and a standby takes over. It loads state from Postgres, bumps every agent's epoch on placement as usual, and nodes reconnect to it.
- Nodes and clients take a comma-separated list of hub URLs and rotate through it, following `x-subnet-leader` hints.
- **Sharding later:** all hub state access goes through `Hub::agent(id)`-style lookups and the work queue, so a future shard router can own a subset of agents per hub.

## Web UI

Vue 3 + Parcel, in `webui/`, embedded into the binary (`rust-embed`, cargo feature `webui`, on by default) and served at `/`.

- **Design:** monochrome and dark. Black background, white/grey text, no colour. State is shown by glyph and pattern: `●` thinking, `▣` tools, `○` idle, `‖` paused, `✕` failed, `·` cancelled. Monospace type throughout.
- **Views:**
  - **Park:** agents laid out as *plots*. Each root agent and its descendants form a bordered block, and plots flow in a responsive grid. Tiles show glyph, type, a token bar and the last line of output, updating live from the event stream. Filters: type, node, phase, route.
  - **Agent panel:** live transcript with the streaming partial, tool calls and results, pending approval (approve/deny), pause (safe/quick/hard), resume, fork, cancel, and a message box.
  - **Senses:** live event feed per sense, and stream status (rate, subscribers).
  - **Switchboard:** routes with counters (matched, dropped, throttled, delivered), and the latest deliveries.
  - **Cluster:** nodes and what they run, current spec version, diff and apply (admin), history.
  - **Inbox:** mail for the logged-in principal.

## TUI

`subnet tui` (ratatui), built on the same API and event stream:

- a park view (tree plots as text blocks)
- an agent pane (transcript and partial)
- key bindings for pause (`s`/`q`/`h`), resume (`r`), cancel (`x`), approve (`a`/`d`) and message (`m`)

## Crates

| crate | purpose |
|---|---|
| `core` | chat types, events, phases, `Agent::apply`, wire protocol, built-in tool defs. No I/O. |
| `llm` | OpenAI-compatible streaming client: retries, usage, partial continuation. |
| `ops` | typed operation registry → MCP server, REST/RPC router + OpenAPI, clap CLI. |
| `cluster` | HCL cluster files: parsing, validation, hashing, diff. No I/O. |
| `switchboard` | CEL evaluation, flow control and route state. No I/O. |
| `subnet` | hub, node (executors, MCP hosting, senses, streams), API/ops wiring, CLI, TUI, embedded web UI. |

## Not in scope yet

- **Sharded active-active hubs** (the structure is prepared, see HA).
- **Direct node-to-node streams.** Streams are relayed through the hub.
- **Per-user visibility of agents.** Every principal sees every agent; roles only gate actions.

## Status

- **Implemented (v1):** core state machine, `llm` client, hub sequencer/placement/fencing/dormancy, internal executor, kill -9 failover tests.
- **Implemented (v2):**
  - `ops` registry with REST/RPC + OpenAPI + docs, MCP and CLI front-ends and an HA-aware client; the hub's operations run on it.
  - `cluster` crate: HCL parsing, validation, identities, node views, diff.
  - Cluster versions (`apply_cluster`, `get_cluster`, `cluster_history`, `rollback_cluster`; `subnet apply` reads files).
  - Principals, roles and tokens (`issue_token`, `revoke_tokens`, `whoami`); `SUBNET_ADMIN_TOKEN` bootstrap; open mode without it.
  - Addresses `user:<name>`, `client:<name>`, `resident:<name>`, `mailbox:<name>`.
  - Nodes: pull-based configuration (`Configure`/`Ready`), credential resolution on the node with errors reported in `list_nodes`, `subnet node`, `subnet dev <files>`.
  - Agent types, mixtures and MCP types from the cluster; tools fixed in the spec at spawn; MCP calls local or routed through the hub with mixture ACLs and remote cancellation.
  - Residents (created when their node is ready, cancelled when removed) and mailboxes (`mailbox_take`/`mailbox_peek`, mixture ACL).
- **Implemented (v2), continued:** external executors (think protocol); parallel tool calls; snapshots; tree forks; event stream (SSE/WS, agent and tree filters); senses on nodes (all sources, stages, same-node streams) with sense events and status in the hub.
- **In progress (v2):** snapshots, tree forks, event filters/SSE, senses/streams/blobs/switchboard, HA, web UI, TUI. Each item moves to "implemented" in the commit that finishes it.
