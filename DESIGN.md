# subagent-net design

A distributed network of LLM agents. Every agent is a resumable state machine. Agents can be paused, resumed, forked and messaged, and they can spawn other agents. The user is one more participant: they spawn agents and talk to any of them.

## Components

```
            user (CLI / TUI / any MCP client)
                         │  MCP (streamable HTTP) + WS
                         ▼
   ┌────────────────────────────────────────────┐
   │ hub                                        │
   │  - sequencer: the only writer of event logs│
   │  - type registry (who offers which type)   │
   │  - agent directory + leases                │
   │  - mailbox routing, event fan-out          │
   └───────┬──────────────────────────┬─────────┘
           │ WS (spawner dials out)   │ Postgres
           ▼                          ▼
   ┌───────────────┐  ┌───────────────┐
   │ spawner A     │  │ spawner B     │   hold API keys + MCP servers,
   │ types: coder, │  │ types: coder, │   run agents: apply() + effects
   │   reviewer    │  │   researcher  │
   └───────────────┘  └───────────────┘
```

- **Hub**: the control plane. It stores all state in Postgres (schema changes go through `sqlx migrate`). It runs no LLM calls and holds no provider keys.
- **Spawner**: a worker process. It dials out to the hub, which works through NAT, and registers the agent types it offers. It runs the agents placed on it. Provider keys and stdio MCP servers stay on the spawner.
- **One binary**, `subnet`, with the subcommands `hub`, `spawner` and the user commands (`spawn`, `send`, `pause`, `tail`, …).

## Agent types

A spawner loads its types from a TOML config and registers them with the hub:

```toml
[[type]]
name    = "coder"
model   = { base_url = "https://api.openai.com/v1", model = "gpt-5", api_key_env = "OPENAI_API_KEY", prefill = false }
system  = "You are a careful Rust engineer..."
mcp     = [{ name = "fs", command = "mcp-fs", args = ["--root", "."] }]
spawns  = ["reviewer"]          # types this agent may spawn
budget  = { max_tokens = 2_000_000, max_depth = 3, max_children = 8 }
```

- The identity of a type is `name@hash(config minus secrets)`. The hub places an agent only on a spawner that offers the exact same type hash, so a resumed agent never switches to a different prompt, model or tool set without anyone noticing.
- A type is available while at least one live spawner offers it. `list_types` shows the types and their capacity.

## Agent = fold over an event log

The hub is the **sequencer**. Spawners *propose* events, and the hub commits them in order and echoes them back. A spawner folds only committed events. Every write carries the owner's **lease epoch**, and the hub rejects writes from an old epoch. This fencing stops a stale spawner from writing after its agent has moved to another one.

```rust
// core (crates/core/src/agent.rs): no I/O, deterministic
impl Agent {
    fn apply(&mut self, ev: &Event) -> Vec<Effect>;
    fn replay(id, spec, events) -> Agent;          // apply without effects
}

enum Event {
    Inbox { from: Addr, content, reply },          // `reply`: an automatic turn-end answer
    LlmDelta { delta }, LlmDone, LlmAborted, LlmFailed { error },
    ToolResult { call_id, content, is_error }, ToolAborted { call_id },
    Approval { call_id, approved },
    ChildSpawned { id, reserved }, ChildReport { id, status, content },
    PauseRequested { mode }, Resumed, Cancelled,
    Recovered,                                     // logged by the hub on every placement
}

enum Effect { CallLlm, CallTool { call, retry }, RequestApproval { call }, AbortInflight,
              Report { to, status, content } }

enum Phase { Idle, Thinking { running }, Tools { queue, wait }, Failed { error }, Cancelled }
enum ToolWait { Ready { retry }, Running, Approval, Approved, Children { ids } }
```

- The agent spec (type, parent, budget, approval list) is stored with the agent, not in the log.
- Tool calls of one assistant message run **one at a time**, so `quick` pause has a meaningful boundary between them.
- `wait_for` is handled by the state machine itself (it waits for `ChildReport` events). Every other tool, built-in or MCP, is executed by the spawner.

**Resuming** means folding the log and continuing from the phase you end up in. Whenever the hub places an agent on a spawner, it first commits `Recovered`: whatever was in flight is gone, and applying `Recovered` returns the effects that restart it. Because recovery is itself logged, the hub's replica and the runner always fold identical state. The same path handles a spawner crash, a hub restart and waking a dormant agent.

**Reports are routed by the hub.** The hub folds every log too, so it performs `Effect::Report` itself, from its own replica. Each report is delivered exactly once, even if the spawner is revoked at that moment (e.g. on cancel).

**Forking** means copying a prefix of the log under a new agent id.

## Pausing

A pause request is a logged event, so it survives crashes and migration to another spawner. A request has a **mode** and a **scope**. The scope is either `self` or `tree` (the agent plus all its descendants).

| mode    | what keeps running                                                         | stops at |
|---------|-----------------------------------------------------------------------------|----------|
| `safe`  | the current task: the whole turn, including tool calls, until the agent would wait for input | turn boundary (`Idle`) |
| `quick` | the in-flight unit: the current LLM stream or MCP call. Queued tool calls are not started. | next step boundary |
| `hard`  | nothing. The LLM stream is dropped; MCP calls are cancelled (`notifications/cancelled`) | immediately |

**Keeping half-responses:**

- The spawner batches stream deltas into `LlmDelta` events, flushing about every 250 ms and always on pause or abort. After a hard pause or a crash, the partial message is exactly the deltas that were committed.
- Resuming from a partial assistant message:
  - If the provider supports it (`prefill = true`, e.g. vLLM `continue_final_message`), the partial is sent as the trailing assistant message and generation continues where it stopped.
  - Otherwise the partial stays in the transcript marked `[interrupted]`, and the model is asked to continue. The spawner logs that this fallback was used.
- A partial tool call (half-streamed JSON arguments) is shown to the user but never executed. It is dropped from the transcript when the agent resumes.
- An aborted MCP call is recorded as `ToolAborted`. On resume, it is re-run if the tool is marked idempotent in the type config. Otherwise the model is told the call was interrupted.

## Messaging and the network

- **Addresses** are `user`, `client:<name>` or `agent:<id>`. A message is an `Inbox` event committed by the hub to the target's log, which makes delivery durable and ordered.
- **Delivery by phase:**
  - `Idle`: the agent wakes up.
  - `Paused`: the message is queued.
  - Any other phase: the message is injected at the next LLM call.
- **Who gets the answer:** when a turn ends, its final message goes to everyone whose message started or joined that turn (the "askers"), plus the parent as a `ChildReport`.
  - An answer delivered to another agent is marked `reply`. It wakes that agent, but its sender is not owed an answer back, so two agents can't ping-pong forever.
  - A turn woken only by replies or child reports answers the previous askers. So the result of a non-blocking spawn still reaches the user who asked for it.
  - A failed turn keeps its askers, so the answer after a resume still arrives.
- **Tools every agent gets** (next to its MCP tools):

  | tool | behaviour |
  |---|---|
  | `spawn_agent(type, prompt)` | non-blocking; only types listed in the parent's `spawns` |
  | `send_message(to, content)` | to an agent, `user` or a client |
  | `wait_for(ids)` | blocks until each child has reported; returns the reports |
  | `list_agents`, `list_types` | |
  | `pause_agent(id, mode, tree)`, `resume_agent(id, tree)`, `cancel_agent(id)` | descendants only |

- **Budgets:**
  - A child's token budget is carved out of the parent's (`ChildSpawned.reserved`), so a subtree can never outspend its root. A child whose type has no limit gets half of what the parent has left.
  - Depth shrinks by one per level; `max_children` is per agent.
  - Cancel always cascades to the subtree; pause and resume do when `tree` is set.
- **Placement:**
  - An agent needs a spawner only while it has work: an LLM call or tool to run, or queued input.
  - Idle, paused, failed and waiting agents (on children or approval) are **dormant**. They keep their slot until another agent needs it, and are placed again when an event gives them work. Thousands of dormant agents cost nothing.
  - An agent goes to the least-loaded live spawner offering its exact type, evicting a dormant agent if all are full.
  - The hub pings spawners. A dead connection moves its agents elsewhere, with the epoch bumped so late writes are fenced.

## User surfaces

- **The hub is an MCP server** at `/mcp` (streamable HTTP). It offers `list_types`, `list_agents`, `spawn`, `send`, `wait_inbox`, `pause`, `resume`, `cancel`, `approve`, `fork` and `transcript`. Claude Code or any other agent can drive the network the way a user would.
  - The caller's address comes from the `x-subnet-as` header: `user`, or a client name giving `client:<name>`. Without the header it is `client:<mcp-session-id>`.
  - Answers are picked up with the blocking `wait_inbox(timeout_ms)` tool. Many MCP clients ignore server notifications, so the design doesn't rely on them.
- **`/events`** (WebSocket) streams committed events live, for all agents or `?agent=<id>`. The CLI's `tail` uses it.
- **Auth:** with `SUBNET_TOKEN` set, spawners present it in their hello, and users/clients as `Authorization: Bearer` (or `?token=` on `/events`).
- **CLI:** `subnet types | agents | spawn [--wait] | send [--wait] | inbox | pause | resume | cancel | approve | fork | transcript | tail`. It talks to the hub through the same MCP endpoint.

## Crates

| crate | purpose |
|---|---|
| `core` | chat types, events, phases, `Agent::apply`, wire protocol, built-in tools. No I/O. |
| `llm` | OpenAI-compatible chat client: SSE streaming, tool calls, partial capture, abort, prefill. Thin serde types that keep unknown fields. |
| `subnet` | library and binary: `hub` (axum, sqlx/Postgres, rmcp server), `spawner` (tokio-tungstenite, rmcp client, `CancellationToken`), `client` + CLI. `subnet dev` runs a hub and a spawner in one process. |

## Not in scope yet

- **Multiple hubs.** There is one hub; Postgres provides durability. Add sharding by agent id if a single hub becomes the bottleneck.
- **Snapshots.** Agents are resumed by folding the full log. Add a snapshot table when replay gets slow.
- **Types that span spawners** (e.g. the model on one machine, MCP tools on another). Not needed until someone asks.
- **Parallel tool calls.** Tools of one message run sequentially. Run them concurrently if latency matters; `quick` pause would then wait for all of them.
- **Tree-filtered event stream.** `/events` filters by one agent or none.
- **TUI.** `tail` and the MCP tools cover it for now.
- **Stale children in forks.** A fork keeps its source's `ChildSpawned` entries, but the children still report to the original agent.

## Milestones (all done)

1. **`llm`**: streaming, tool-call round trip, partial capture on abort, prefill continuation. Tested against a mock SSE server.
2. **`core`**: `Agent::apply` and fold. Tests for every pause mode × every phase, plus fold determinism.
3. **hub**: migrations, sequencer with epoch fencing, spawner register/heartbeat/lease, WS protocol.
4. **spawner**: type config, placement, effect executor with cancellation, delta flushing.
5. **MCP tools**: rmcp clients per type, cancellation, idempotency flag, approval gate.
6. **network**: spawn/send/wait tools, budgets, cascading pause and cancel.
7. **user surfaces**: hub as MCP server (including `wait_inbox`), CLI, `tail`.
8. **failover test**: kill a spawner mid-stream. The agent must resume on another spawner with its partial output intact.
