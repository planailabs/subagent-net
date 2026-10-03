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
- **One binary**, `subnet`: `hub`, `node`, `dev` (hub and node in one process), `tui`, `watch <id>` (one agent's transcript, live and readable), and every API operation as a CLI command.
- **Environment files:** before parsing its flags (several read env defaults: `DATABASE_URL`, `SUBNET_HUB`, `SUBNET_TOKEN`, …), `subnet` loads `--env-file PATH` (repeatable) and then `./.env`. A variable that is already set is never overwritten: the real environment wins, then earlier files. Nodes resolve credentials (`$VAR`) from the result, so API keys can live in a node's `.env`.

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
  group_events = true                         # optional: events from routes in one message per call (below)
  search_history = true                       # optional: the built-in search_history (below)
  compact  = { at_tokens = 64000, keep = 8 }  # on by default (96000, 8); enabled = false turns it off;
                                              # instructions = "…" adds what its summaries must keep;
                                              # prompt = "…" replaces the built-in instructions
  vision   = { max_px = 1024 }                # its model sees images from tools (formats, max_px, keep)
}

# --- MCP servers ------------------------------------------------------------
mcp "memory" {
  command    = ["mcp-memory", "--db", "/var/lib/memory"]   # stdio
  env        = { LOG = "warn", TOKEN = "$MEMORY_TOKEN" }  # $VAR or ${VAR} = node env
  nodes      = ["gpu-1"]
  idempotent = ["search"]
  lazy       = false                                      # schemas always offered
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
  router { top_k = 3 }                       # pre-load matching lazy tools
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
route "night-desk" {                          # through a hold: kept while it's frozen
  from = "door"
  hold = "night"
  deliver { send = "concierge" }
}
route "lights-out" {
  from = "door"
  when = "event.state == 'locked'"
  deliver { freeze = "night" }
}
route "morning" {
  from = "door"
  when = "event.state == 'unlocked'"
  deliver { release = "night" }
}
```

- Identity of an agent type or MCP type is `name@hash`. The hash covers everything except resolved secrets and `nodes`. An agent is only ever resumed on a node offering the same hash: changing an agent type (or an MCP server of its mixture) in the cluster leaves existing agents of the old version pending. They're listed `outdated`; `upgrade` moves one onto the current version (see Forking and upgrading). Residents move by themselves.
- A `resident` is created when its type is available, answers to whoever applied the cluster, and is cancelled when removed from the file. When the cluster changes its type (or its MCP servers), it's upgraded once a node offers the new version: same history, new agent, same resident name. The resident's name points at the copy before the old agent stops, and a message sent to a superseded agent goes on to its copy, so nothing sent during the move is lost. Cancelling a resident's agent starts the resident afresh (a new agent with the resident's prompt). Changing which MCP servers its mixture lists doesn't replace the running agent.
- `$VAR` in values and `env = "..."` in credentials are resolved on the node. A missing variable makes that type unavailable on that node, reported back to the hub and shown by `subnet list-nodes`.
- Durations are `250ms`, `10s`, `5m`, `2h`, `1d`. Rates are `N/duration`.
- HCL allows one attribute per line inside a block, so multi-field values on one line use object syntax: `budget = { max_tokens = 1000, max_depth = 2 }`.
- Several files can be applied together; each `kind "name"` may be declared once across them. Unknown blocks or fields are errors.
- The example above is parsed and validated by the `cluster` crate's tests, so it stays correct.
- Validation happens at apply time. Unknown references (a route to a missing mixture, a sense on an undeclared node) reject the whole version.
- `subnet apply --dry-run` prints the diff against the current version. `subnet cluster-history` and `subnet rollback-cluster <version>` manage versions.

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
    PauseRequested { mode }, Resumed, Cancelled, Superseded { by },
    Recovered,                                     // logged by the hub on every placement
    ToolsLoaded { names },                         // the tool router
    Compacted { upto, summary, usage }, CompactFailed { error },
}

enum Effect { CallLlm, Compact { upto }, CallTool { call, retry }, RequestApproval { call },
              AbortInflight, Report { to, status, content } }

enum Phase { Idle, Thinking { running }, Tools { calls }, Failed { error }, Cancelled }
// each pending tool call has its own state:
enum CallState { Queued, Running, Approval, Approved, Children { ids }, Done }
```

- The spec (mixture, agent type, MCP types, parent, budget, approval list, tool list) is stored with the agent, not in the log.
- **Parallel tools:** all tool calls of one assistant message start together (each may first need approval). The next LLM call happens once all have results. A `quick` pause lets the running calls finish but starts no queued or approved ones.
- `wait_for` is handled by the state machine itself (it waits for `ChildReport` events). Every other tool is executed by the node.
- **Recovery:** whenever the hub places an agent, it first commits `Recovered`: whatever was in flight is gone, and applying `Recovered` returns the effects that restart it. Because recovery is logged, every replica folds identical state.
- **Reports are routed by the hub** from its own replica, exactly once.
- **Compaction** (on by default; see below) keeps long conversations within the model's context.
- **Snapshots:** each time an agent's log crosses a multiple of 200 events the hub stores its folded state (`agent_snapshots`, newest only). Loading an agent reads the snapshot and the events after it; `Assign` carries the snapshot and only the events after it (never a snapshot taken at the final `Recovered`, so the node always has an event to start from).
- **Forking:** `fork(id, at, tree)` copies the log prefix into a new agent. Copied history is folded without re-running its effects, so old answers aren't delivered again.
- **Upgrading:** `upgrade(id, tree)` moves a root agent onto the current version of its type: its whole log is copied into a new agent whose spec is built from the cluster as it is now (type, tools, MCP servers, compaction; its budget and tenant are kept), the old agent and its descendants get `Superseded { by }` (they stop like a cancel but report nothing: the work goes on in the copy), and a resident pointing at it points at the copy. With `tree` its children move too; without, they stop. Children aren't upgraded on their own (their parent knows them by id).
  - Without `tree`, the copy has no children: `ChildSpawned`/`ChildReport` events are left out, so a `wait_for` pending at the fork point returns "not a child".
  - With `tree`, every child spawned within the prefix is forked as well (recursively, with its full log), with ids remapped in all copied events and specs.

### Compaction

Each LLM call reports its context size (prompt + completion tokens). Before the next call, if that reached the agent type's `compact.at_tokens` (default 96 000), the state machine asks for a compaction (`Effect::Compact { upto }`) instead of the call:

- **What goes:** everything but the first message (the task) and the last `keep` messages (default 8). The kept part never starts with a tool result, so a call stays with its result. It only happens between steps, when no tool call is pending.
- **The summary:** the node asks the agent's own model, without tools, to summarise the conversation up to `upto` (`Agent::compaction_request`: fixed instructions (`COMPACT_PROMPT`), then the type's own `compact.instructions` if it has any, the agent's system prompt, the transcript as text). A type's instructions say what else its summaries must keep, or may drop (a character: feelings and relationships; a researcher: every source). Or a type gives its own `compact.prompt` in place of the built-in instructions (an agent that isn't working on a task: a character's memory of her days); `COMPACT_CONTRACT` (the agent continues from the summary plus its kept messages; answer with the summary only) always follows it. A type has one or the other, not both. Either is part of the type: changing it is a new version, as with any setting; unset, they leave a type's id as it was. It proposes `Compacted { upto, summary, usage }`.
- **Applying it:** messages `1..upto` become one user message with the summary. Schemas the agent loaded (`load_tools`) come back as a note after it, since `call_tool` still needs them. The summary call's usage counts towards the budget. The LLM call then goes ahead.
- **Deterministic:** the summary is in the log, so replicas fold the same transcript. After a crash mid-summary, recovery asks for it again.
- **Failures are not fatal:** `CompactFailed` (the model errored or returned nothing) lets the call go ahead uncompacted; the next step tries again.
- The provider's prompt cache restarts after a compaction (the prefix changed); that's the price.
- **Nothing is deleted:** compaction changes what the model is sent, not the log. Every event stays in `events` (append-only), so the whole conversation can be read back: `transcript` and `watch_agent` with `full = true` replay the log (`Agent::full_history`) into the messages as they happened, compacted ones included (each compaction's own notes left out), and `compacted: [{from, to, summary}]` says which were replaced by which summary. A full transcript only grows, so a full watcher's cursor never goes back. The web UI's agent panel shows it, each compaction folded (its summary, then the messages it replaced). Blobs an agent's log mentions (`blob:<sha256>`) aren't collected, so its pictures stay too.
- Watchers (not `full`) whose cursor is past the end of a compacted transcript start again from the top. Agent summaries show `compactions`.
- `compact { enabled = false }` turns it off for a type; external executors never compact (their brain owns its context).
- **Compacting by hand:** `compact(id)` (API `POST /v1/agents/{id}/compact`, the CLI, MCP; the web UI's agent panel; `c` in the TUI) logs `CompactRequested`: a compaction regardless of the context's size (with compaction off too, keeping the last `DEFAULT_KEEP` = 8 messages). An idle agent compacts at once and is idle again after (no model call follows unless messages came meanwhile; after a crash only the compaction is asked for again); a busy one compacts before its next model call. With too little to summarise, nothing happens.

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

**Failing and resuming.** An agent fails when a model call fails for good (after the client's retries, or at once for an error that won't pass, like a provider's "insufficient balance"), or when its budget is used up. Agent summaries say why (`error`; the web UI and TUI show it). `resume` goes on from where it failed, the partial answer and the queued messages included: once the cause is gone (the provider has credit again), nothing is lost. A resident whose agent failed stays failed until someone resumes it; upgrading it carries the failed state over (resume the new agent).

**Half-responses are kept.** Streamed deltas are logged in batches, so after a hard pause or crash the partial message is whatever was committed. On resume the partial is continued: natively with `prefill = true` (vLLM `continue_final_message`), otherwise it is marked `[interrupted]` and the model is asked to go on. Half-streamed tool calls are never executed. An aborted tool call becomes `ToolAborted`; after a crash it is re-run only if it is idempotent.

### Messaging

- **Addresses:** `user:<name>`, `client:<name>`, `agent:<id>`, `resident:<name>` (resolves to the resident's agent), `mailbox:<name>`, `route:<name>` (a switchboard route as sender; answers to it are kept there).
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
  | `load_tools(names)`, `call_tool(name, arguments)` | load lazy tools' schemas and call them (see [Lazy tools](#lazy-tools)); offered when a mixture has lazy tools |
  | `mailbox_take(name, max)` / `mailbox_peek(name, max)` | mailboxes the mixture lists |
  | `blob_get(ref)` | fetch a blob as text/base64 |
  | `search_history(pattern, page?)` | only for a type with `search_history = true`: its own whole conversation, the parts compaction summarised away too (the full history, as `transcript` with `full` has it), searched with a case-insensitive regex; the matching messages oldest first, 20 a page, each `{n, role, summarised, text}` (`text`: the part around the match; `summarised`: in a compaction's range), its own searches left out |
  | `list_agents`, `list_types` | |
  | `pause_agent`, `resume_agent`, `cancel_agent` | descendants only |

- **Bugs in the state machine stay contained:** a panic in `Agent::apply` is caught. That agent fails with an internal error (its askers get the failure, in-flight work is aborted), and the hub, the node and every other agent carry on. Replicas and replays run the same code, so they fail the same way; the agent can be resumed. `subnet_core::agent::in_guarded_apply()` lets a process-wide panic hook tell these apart from uncaught panics.
- **Token usage** counts prompt and completion tokens, and the prompt tokens the provider served from its cache (`cached_prompt_tokens`, read from `prompt_tokens_details.cached_tokens` or DeepSeek's `prompt_cache_hit_tokens`). An agent type's `budget { cached_percent = 10 }` counts cached tokens at that percentage towards `max_tokens` (unset: 100), so budgets can track cost; cached input costs about a tenth.
- **Budgets:** a child's token budget is carved out of the parent's (`ChildSpawned.reserved`). A child gets at most half of what the parent has left (a type without a limit gets exactly that), so a parent never spends its whole budget on children and can still read their answers. A spawn whose child would get less than a tenth of its type's limit is refused, as a tool error the model sees. Depth shrinks per level; `max_children` is per agent.
- **Placement:** an agent needs an executor only while it has work. Dormant agents keep their slot until it's needed, and are placed again when an event gives them work. Placement picks the least-loaded live node offering the exact type hash.

## MCP servers and the MCP switchboard

- An `mcp` block declares an MCP type and the nodes that run it. On connect, a node starts its MCP servers (stdio or HTTP) and reports each type's tool list to the hub.
- A **mixture** binds an agent type to MCP types. A spawned agent's tool list is the built-ins plus the tools of its mixture's MCP types, named `<mcp>.<tool>` (e.g. `memory.store`). The list is fixed in the spec when the agent is created, so replays are stable. When a server of the same version offers other tools later (it grew one, or dropped one), the hub notices it as a node reports what it runs and logs `ToolsChanged { tools, idempotent, lazy, added }` for each live agent using it: the model is offered the new list from then on and told of the added ones ("[new tools you have now: …]" before its next message). Another version of a server is an upgrade, not this. On the wire to the model, names are `<mcp>__<tool>` (OpenAI-compatible APIs only accept `[a-zA-Z0-9_-]`); the LLM client translates both ways, and the state machine accepts either form in tool calls and `load_tools`.
- **Routing a tool call:**
  - If the agent's node runs that MCP type, the call is local.
  - Otherwise the node sends `McpCall { call_id, agent, epoch, mcp, tool, args }` to the hub. The hub checks that the agent's mixture includes that MCP type and forwards the call to the least-loaded node running it. The result comes back the same way.
  - Hard pause turns into `McpCancel`, which reaches the server as `notifications/cancelled`.
- Routes can call MCP tools directly with event data (`deliver { mcp { … } }`). The hub performs these calls like any other, with retries for idempotent tools.

### Tenants

An agent can do work *for* someone: its **tenant**. A tenant is set when an agent is spawned from outside the cluster (`spawn` with `tenant`), and every descendant inherits it; agents can't choose one for their children. Agent summaries show it.

An MCP server with `per_tenant = true` (stdio only) runs once per tenant: `${TENANT}` in its `env` is the tenant of the calling agent, so each tenant's calls go to a process started with that tenant's settings (credentials, a data directory, an identity). It needs a `default_tenant`: that instance starts with the node, lists the tools and serves agents without a tenant. A node starts a tenant's instance on its first call and stops it after 10 idle minutes; calls routed through the hub carry the caller's tenant to the node that runs the server.

```hcl
mcp "notes" {
  command        = ["notes-mcp"]
  env            = { NOTES_DIR = "/srv/notes/${TENANT}" }
  per_tenant     = true
  default_tenant = "shared"
  nodes          = ["gpu-1"]
}
```

`env` values are literals, `$VAR` (the node's variable), or strings with `${VAR}` inside (`${TENANT}` included).

### Lazy tools

MCP tools are **lazy** by default: the model sees only their names until it loads them, which keeps big servers (dozens of tools) out of every prompt. `lazy = false` on an `mcp` block offers its full schemas from the start.

The tools offered to the model **never change** during an agent's life. Providers cache the prompt prefix (DeepSeek and OpenAI put the tool list before the conversation), so changing the tools would make every later call re-read the whole conversation uncached. Measured on DeepSeek: 8,064 of 8,293 prompt tokens cached on a repeat; 128 after adding one tool. So:

- Each LLM call offers the built-ins, the eager tools and, if there are lazy tools, `load_tools` and `call_tool`. `load_tools`' description is a fixed catalogue of every lazy tool (`- web.search: <first sentence>`).
- `load_tools { names }` takes tool names or a server name (all its tools). The state machine resolves it itself, like `wait_for`: no node round trip. The result (`loaded …`, one JSON schema per line, `unknown: …`) goes into the conversation, where the model reads the schemas.
- `call_tool { name, arguments }` calls a loaded tool. The state machine turns it into the call of the tool it names (same call id) when the call is dispatched. Approvals, idempotent retries and crash recovery all see the real tool. Calling a loaded lazy tool by its own name works too.
- A lazy tool called before it's loaded doesn't run: it gets loaded, and the result is its schema and a request to call again.
- The spec lists the lazy tool names (`Spec.lazy`); the agent's state keeps what's loaded (`Agent.loaded`), so replay, resume and forks keep it.

### The tool router (optional)

A mixture with a `router { top_k = 3, min_score = 0.78 }` block gets matching lazy tools pre-loaded for every message the hub delivers to its agents (`send`, route deliveries, a spawn's prompt; not reports). The hub embeds the message and each unloaded tool's name and description and appends `ToolsLoaded { names }` right before the `Inbox` event. The loaded schemas become a note in the conversation just before the message, so the model has them on its first call and the tool list stays the same.

- Similarity is cosine over multilingual e5-small embeddings (fastembed, downloaded into `$SUBNET_MODELS`, default `subnet-models/`, on first use; the `router` cargo feature). Tool embeddings are cached. e5 puts scores in a narrow band, so `min_score` is a coarse filter: a wrong pre-load costs a few schema tokens, and a missed one is still a `load_tools` away.
- Without a model (download failed, feature off) the router logs a warning and pre-loads nothing.

### Images and vision

- **From tools:** an MCP result's image (and audio) blocks are stored in the blob store by the agent's node, and the tool result carries a marker instead of the bytes: `[image blob:<sha256> image/jpeg 800x600]` (`[blob:<sha256> audio/wav]` for other media). Event logs and transcripts stay small; `blob_get` fetches the bytes.
- **Seeing:** an agent type with `vision = { formats = [...], max_px = 1568, keep = 3 }` (`vision = {}` for these defaults: png, jpeg, webp, gif) has a model that takes images. Its node converts each tool image to a format the model accepts (JPEG if allowed, else PNG, else the first one listed) and scales it to at most `max_px` on its longer side before storing it. For every call it attaches the conversation's last `keep` images: a user message carries its own, and a tool result's go in a user message after the run of tool results (chat APIs take images only from users), as OpenAI-style `image_url` data URLs. Images it can't fetch stay markers. Without `vision` the model sees only the markers.
- **To tools:** an argument whose JSON schema says `"format": "blob"` (a string, or an array's items) takes `blob:<sha256>` references; the node replaces them by `data:<mime>;base64,…` URLs before the call, so an agent can hand a tool what it saw (say, to keep with a memory).
- **From events:** an image sent inline in a sense event (`{"$blob": {"base64", "mime": "image/…"}}`, e.g. a picture attached to a chat message) becomes a marker the same way, so a model that sees gets it; it is fitted (converted, scaled) when attached, as it came as it was sent.
- Nodes keep the last images they stored, and fetch others from the hub (`BlobRaw`, an op only nodes use).

## Senses, streams and the switchboard

### Events and blobs (durable)

A sense emits **events**: `{ id, sense, at, data }`. `data` is JSON. Large payloads go into the **blob store** (Postgres `blobs` table, 64 MiB limit per blob): a sense writes `{"$blob": {"base64": …, "mime": …}}` anywhere in its output, and the node uploads the bytes and replaces that object with `"blob:<sha256>"` before sending the event. Blobs are content-addressed and read via `blob_get` (API: base64; `GET /v1/blobs/<hash>/raw`: bytes; agents: the `blob_get` tool, text as-is or base64, cut at 256 KiB). `blob_put` stores one from a client. A blob is deleted 7 days after it was last stored or read, unless an agent's log mentions it (`blob:<sha256>` in any event: a picture it saw, a reference it was sent); those stay with the history.

### Streams (ephemeral)

A sense whose source declares `stream = "<format>"` publishes a **binary stream** named after the sense instead of events.

- A sense with `source { stream = "<name>" }` subscribes to it.
- On the same node, streams are in-process pipes. Across nodes, the hub relays them over a dedicated WebSocket per node (`/streams?node=…&token=…&subscribe=a,b`), so audio frames never delay control traffic. Frames are multiplexed as `[name length: u8][name][bytes]`. The cluster view tells each node which streams to send (`relay_out`: published here, subscribed elsewhere) and which to receive (`relay_in`). In-process nodes (`subnet dev`) use the hub's relay directly.
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
4. **Deliver** (a route may have several). Deliveries are sent as `route:<name>`:
   - `spawn = "<mixture>"`, `prompt = CEL`: a new agent per delivery (per batch). Without `prompt` the agent gets the event JSON. The agents' answers are kept at the address `route:<name>` (read them with `peek_mail`).
   - `send = "<resident>"`: an `Inbox` message with the event JSON; the resident's answer also goes to `route:<name>`.
   - `mailbox = "<name>"`: durable queue; agents read it with `mailbox_take`.
   - `mcp { server, tool, args = CEL }`: a direct tool call on any node running the server; idempotent tools are retried (3 attempts). Without `args` the tool gets `{"event", "batch"}`.
   - `max_active` counts a route's spawned agents until their first answer; slots are reserved atomically, excess spawns wait in the route's queue.
   - `freeze = "<hold>"` / `release = "<hold>"`: freeze or release a hold (below).

**Holds (freeze and release).** A route with `hold = "<name>"` delivers through that hold. While the hold is **frozen**, the route's deliveries (after its flow control) are kept in it instead, in the order they came, across all its routes (at most 1000; the oldest go and are counted as dropped); when it's **released**, what it kept is delivered, in that order, and its routes deliver again. Every route through a hold delivers through one lane, in the order dispatched, so a release's backlog always arrives before anything after it. Holds are frozen and released by deliveries (`freeze`, `release`: a route that does so can't itself go through a hold, and its hold must be one some route goes through) or by hand (`freeze_hold`, `release_hold`; `list_holds` shows each hold: frozen, since, how many wait, dropped, its routes). An event's freezes and releases happen before its other deliveries: an event that releases a hold arrives after what the hold kept, and one that freezes it is itself kept. Holds and what they keep are in Postgres (`holds`, `held`), written in order; a new leader picks them up. A use: two residents chatting can be frozen once they've said enough, everything for them (their lines, world events, timers) waiting, and released when a person speaks.

**Grouped events:** an agent type with `group_events = true` gets the events routes `send` it together: everything queued for its next model call from routes becomes one message, grouped by route in the order they first came, each event on its own line (JSON compacted; other text with its line breaks escaped), the groups separated by a blank line:

```
[events from route:chat-to-vesper]
{"from":"alice","text":"hi"}
{"from":"bob","text":"hello"}

[events from route:world-to-vesper]
{"event":"joined","who":"bob","text":"bob came into the room."}
```

Messages from people, agents and replies stay messages of their own. Without it (the default) each event is a message, `[message from route:x]`. It's part of the type (a new version); unset, a type's id is unchanged.

Every delivery is recorded (`deliveries` table: route, payload, one outcome per action) and streamed as a `delivery` notice. Counters per route (seen, filtered, deduped, debounced, throttled, delivered, errors) come from `list_routes`. Route state (throttle windows, dedupe keys, debounce timers, open batches) lives in memory on the leader; after a failover, open batches and debounce windows restart empty.

`inject_event` feeds an event into the switchboard as if a sense had produced it (testing routes, the web UI).

## APIs: one registry, four front-ends

Every operation is defined once in the `ops` crate's typed registry: name, summary, argument and result types (serde + schemars), required role, and optionally a REST route. From the registry the hub serves:

- **REST + RPC API:** `POST /v1/ops/<name>` for every op, plus REST routes where declared, e.g.
  - `GET /v1/agents`, `GET /v1/agents/{id}`
  - `POST /v1/agents` (spawn), `POST /v1/agents/{id}/pause`
  - `GET /v1/cluster`, `PUT /v1/cluster` (apply)

  `GET /v1/openapi.json` is the generated OpenAPI 3.1 document; `/v1/docs` renders it.
- **MCP** (`/mcp`): every op a principal's role allows is a tool.
- **CLI:** every op is a `subnet` subcommand (`list_agents` → `subnet list-agents`). Path parameters are positional; an op without path parameters takes its required scalar fields positionally (`subnet spawn <type> <prompt>`). Other fields are flags, and `--json` passes a whole argument object. The CLI calls the REST/RPC API.
- **Events:** `GET /v1/events` (SSE) and `/v1/events/ws` (WebSocket), filtered by `agent`, `tree` (an agent and its descendants), `sense`, `route`, or `agents=true|false`. Each notice has a `kind`: `agent` (`agent`, `ancestors`, `seq`, `event`), `sense` (`sense`, `node`, `id`, `at`, `data`) or `delivery` (`route`, `payload`, `outcomes`). A slow subscriber gets `{"kind":"lagged","missed":n}` instead of blocking the hub.

- **Watching an agent** (`watch_agent`, `GET /v1/agents/{id}/watch`, the MCP tool of the same name): a cursor long-poll for callers that can't hold a stream open. It takes `after`, the cursor from the last call. It returns the transcript entries from there (role, content, tool calls with arguments, the call a result answers), the streamed partial, the agent's state and queued-message count, and `next`. With nothing new it waits (`timeout_ms`, default 25 s, max 120 s) until any event of the agent (a streamed delta included) and then 150 ms more to batch a burst. Without a cursor it answers at once with the last `tail` (default 20) entries. Texts and arguments longer than `max_chars` (default 2000, 0 = no cap) are cut and marked `truncated`. It subscribes to notices before reading, so nothing between the read and the wait is missed. `subnet watch <id>` is built on it: user messages, the answer as it streams, tool calls (→) and results (←), and state changes (·), coloured on a terminal.

The web UI and the TUI use the same API.

## Auth

- Principals are declared in the cluster file: `user`, `client`, `node`. Each has a role:
  - `admin`: apply cluster files, issue tokens, everything else
  - `operator`: spawn, send, pause, resume, cancel, compact, approve, fork, upgrade
  - `viewer`: read only
  - nodes: the node protocol only
- `subnet issue-token <kind> <name>` (admin) creates a random token. The hub stores only its SHA-256.
- The hub's `SUBNET_ADMIN_TOKEN` env var is a built-in `user:root` admin for bootstrap. Without it the hub runs in **open mode** (development): every caller without a known token is `user:root`.
- A principal removed from the cluster file loses access immediately; its tokens stop resolving.
- API and MCP use `Authorization: Bearer <token>`. The web UI exchanges a token for an HTTP-only session cookie (`POST /v1/login`).
- A caller's address is its principal: `user:maciej`, `client:claude`.

## High availability

- Hubs share one Postgres. Each hub competes for `pg_try_advisory_lock` on a dedicated connection (retrying every 200 ms); the holder is the **leader**.
- On winning, the leader bumps `hub_leader.term`, records its advertised URL (`subnet hub --advertise <url>`, default `http://<listen>`), loads all state from the database, and serves. Nodes and clients reconnect to it; agents get new epochs on placement as usual.
- **Fencing:** every event append checks, in the same transaction, that `hub_leader.term` is still this hub's term. A deposed leader (lost its lock connection, partitioned) can't write agent logs even if other connections still work; its first refused write makes it step down.
- A leader that loses its lock connection, or is fenced, steps down: it drops all in-memory state, closes node connections, and becomes a standby again.
- **Standbys** answer every request with `503` and `x-subnet-leader: <url>` when they know the leader. The API client and nodes take a comma-separated list of hub URLs, follow the hint, and otherwise rotate through the list.
- `Hub::open` (single hub, tests) waits to lead; `subnet hub` starts as a standby and leads once elected. `shutdown` releases leadership.
- Route state (throttle windows, open batches, debounce timers) is in memory and restarts empty on the new leader.
- **Sharding later:** all hub state access goes through `Hub` methods and the commit/placement work queue, so a future shard router can own a subset of agents per hub.

## Web UI

Vue 3 + Parcel, in `webui/`, embedded into the binary (`rust-embed`, cargo feature `webui`, on by default) and served at `/` (unknown paths get the app). The build embeds `webui/dist` if you've built it (`npm run build`). Otherwise, as in a git dependency's checkout, the build script builds the UI into cargo's `OUT_DIR` when npm is on PATH (`SUBNET_WEBUI_BUILD=0` skips that). Without npm it embeds a page saying the UI isn't built; the Rust build never needs node. Sign-in: `POST /v1/login {token}` sets an HttpOnly, SameSite=Strict session cookie that the API, MCP and event stream accept (`EventSource` can't send headers); `POST /v1/logout` clears it. Pure logic (park layout, notice folding) lives in `webui/src/lib` and is tested with `node --test`.

- **Design:** monochrome and dark. Black background, white/grey text, no colour. State is shown by glyph and pattern: `●` thinking, `▣` tools, `○` idle, `‖` paused, `✕` failed, `·` cancelled. Monospace type throughout.
- **Views:**
  - **Park:** agents laid out as *plots*. Each root agent and its descendants form a bordered block (a tree in spawn order), and plots flow in a responsive grid with live plots first. Tiles show glyph, type, a token bar and the last line of output (streaming, else the last answer), updating live from the event stream. A text filter matches type, id, phase, node and "paused". `#park/<id>` opens an agent.
  - **Agent panel:** live transcript with the streaming partial, tool calls and results, pending approval (approve/deny), a failed agent's reason (`error`) with resume, pause (safe/quick/hard), resume, compact, fork, cancel, an upgrade button when the agent is outdated (the park marks it ⇡), and a message box.
  - **Senses:** live event feed per sense, and stream status (rate, subscribers).
  - **Switchboard:** routes with counters (matched, dropped, throttled, delivered), the holds (frozen or open, how many wait, a freeze/release button), and the latest deliveries.
  - **Cluster:** nodes and what they run, current spec version, diff and apply (admin), history.
  - **Inbox:** mail for the logged-in principal.

## TUI

`subnet tui` (ratatui), built on the same API and event stream:

- a park view: plots as headed groups, agents as tree rows with glyph, type, token bar and last line
- an agent pane: transcript, streaming partial and pending approval, scrolled to the end
- keys: `j`/`k` select, `s`/`q`/`h` pause safe/quick/hard, `r` resume, `x` cancel, `c` compact, `a`/`d` approve/deny, `m` message, `f` fork, `esc` quit

State and key handling (`tui::App`) are pure and tested; rendering is tested against ratatui's `TestBackend`.

## Crates

| crate | purpose |
|---|---|
| `core` | chat types, events, phases, `Agent::apply`, wire protocol, built-in tool defs. No I/O. |
| `llm` | OpenAI-compatible streaming client: retries, usage, partial continuation. |
| `ops` | typed operation registry → MCP server, REST/RPC router + OpenAPI, clap CLI. |
| `cluster` | HCL cluster files: parsing, validation, hashing, diff. No I/O. |
| `switchboard` | CEL evaluation, flow control and route state. No I/O. |
| `subnet` | hub, node (executors, MCP hosting, senses, streams), API/ops wiring, CLI, TUI, embedded web UI. |

[personality-example/](personality-example/) is a worked example built on these crates: Vesper, a virtual personality in a 3D room, with its own [DESIGN.md](personality-example/DESIGN.md). Its crates share this workspace.

## Not in scope yet

- **Sharded active-active hubs** (the structure is prepared, see HA).
- **Direct node-to-node streams.** Streams are relayed through the hub.
- **Per-user visibility of agents.** Every principal sees every agent; roles only gate actions.

## Status

Everything in this document is implemented, except what "Not in scope yet" lists:

- **Agents:** core state machine (pause modes, recovery, approval, children, budgets, parallel tools, compaction), vision (images from tools, converted and attached for models that see; blob arguments), tenants and per-tenant MCP servers, snapshots, forks (with tree), upgrades onto a type's new version (residents automatically), internal and external executors.
- **Hub:** sequencer, placement with dormancy and eviction, epoch fencing, reports, mailboxes, residents, MCP routing with mixture ACLs, blob store, active-standby HA with term fencing.
- **Cluster files:** parsing, validation, identities, node views, diff, versions and rollback.
- **Nodes:** pull-based configuration, credential resolution, MCP hosting, senses (all sources and stages), stream relay.
- **Switchboard:** CEL, flow control, all delivery kinds, holds (freeze and release, kept in Postgres), deliveries log.
- **Surfaces:** ops registry (REST/RPC + OpenAPI + docs, MCP, CLI, client), principals/roles/tokens, event stream (SSE/WS), web UI, TUI.

When something in this document changes, the change and its status land in the same commit.
