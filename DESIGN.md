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
   │ types: coder, │  │ types: coder, │   run agents: step() + effects
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
// core: no I/O, deterministic
fn step(state: &Agent, ev: &Event) -> (Agent, Vec<Effect>);

enum Event {
    Created { ty, parent, spec },
    Inbox { from: Addr, content },                 // user or agent message
    LlmDelta { text, tool_call_frags },            // batched stream chunks (kept for partials)
    LlmDone { message },                           // complete assistant message
    LlmAborted,                                    // hard pause/crash: deltas so far are the partial
    ToolStarted { call_id }, ToolResult { call_id, result }, ToolAborted { call_id },
    ChildSpawned { id }, ChildDone { id, summary },
    PauseRequested { mode, scope }, Resumed,
    Cancelled, Failed { error },
}

enum Effect { CallLlm, CallTool { call_id }, AbortInflight, Spawn { ty, prompt },
              Send { to, content }, ReportDone { summary } }

enum Phase { Idle, Thinking, RunningTools { pending }, AwaitingChildren { ids },
             AwaitingApproval { call_id }, Paused { mode, resume_to }, Done, Failed }
```

**Resuming** means folding the log and continuing from the phase you end up in. The same code path handles a restart, a spawner crash (the hub reassigns the agent) and a user-initiated resume.

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

- **Addresses** are `user`, `client:<mcp-session>` or `agent:<id>`. A message is an `Inbox` event committed by the hub to the target's log, which makes delivery durable and ordered.
- **Delivery by phase:**
  - `Idle`: the agent wakes up.
  - `Paused`: the message is queued.
  - Any other phase: the message is injected at the next step boundary.
- **Tools every agent gets** (served the same way as MCP tools):

  | tool | behaviour |
  |---|---|
  | `spawn_agent(type, prompt)` | non-blocking; only types listed in the parent's `spawns` |
  | `send_message(to, content)` | |
  | `wait_for(ids)` | the agent enters `AwaitingChildren` |
  | `list_agents` | |
  | `pause(id, mode)` | |
  | `cancel(id)` | |

  A child's final answer arrives in its parent's inbox as `ChildDone`.
- **Budgets** (tokens, depth, number of children) are taken from the parent's remaining budget, so a subtree can never outspend its root. Cancelling or hard-pausing an agent with `tree` scope cascades to all its descendants.
- **Placement:** when an agent is spawned or woken, the hub picks a live spawner that offers the type and has free capacity. Spawners send heartbeats. When a lease expires, the hub bumps the epoch and reassigns the agent.

## User surfaces

- **The hub is an MCP server** (`rmcp`, streamable HTTP). It offers `list_types`, `spawn`, `send`, `pause`, `resume`, `fork`, `cancel`, `tree` and `transcript`. An external agent such as Claude Code can use it to drive the network the way a user would.
  - Each MCP session gets its own address, `client:<session>`, so agents can reply to that client specifically.
  - Replies are picked up with a blocking `wait_inbox(timeout)` tool. Many MCP clients ignore server notifications, so the design doesn't rely on them.
- **WS subscription** streams live events (tokens, phase changes, tool calls) for one agent or a whole tree. The CLI `tail` command uses it, and a TUI can be built on it later.

## Crates

| crate | purpose |
|---|---|
| `core` | events, phases, `step()`, wire protocol types. No I/O. |
| `llm` | OpenAI-compatible chat client: SSE streaming, tool calls, partial capture, abort, prefill. Thin serde types that keep unknown fields. |
| `subnet` | binary: `hub` (axum, sqlx/Postgres), `spawner` (tokio-tungstenite, rmcp client, `CancellationToken`), `cli` |

## Not in scope yet

- **Multiple hubs.** There is one hub; Postgres provides durability. Add sharding by agent id if a single hub becomes the bottleneck.
- **Snapshots.** Agents are resumed by folding the full log. Add a snapshot table when replay gets slow.
- **Types that span spawners** (e.g. the model on one machine, MCP tools on another). Not needed until someone asks.

## Milestones

1. **`llm`**: streaming, tool-call round trip, partial capture on abort, prefill continuation. Tested against a mock SSE server.
2. **`core`**: `step()` and fold. Tests for every pause mode × every phase, plus fold determinism.
3. **hub**: migrations, sequencer with epoch fencing, spawner register/heartbeat/lease, WS protocol.
4. **spawner**: type config, placement, effect executor with cancellation, delta flushing.
5. **MCP tools**: rmcp clients per type, cancellation, idempotency flag, approval gate.
6. **network**: spawn/send/wait tools, budgets, cascading pause and cancel.
7. **user surfaces**: hub as MCP server (including `wait_inbox`), CLI, `tail`.
8. **failover test**: kill a spawner mid-stream. The agent must resume on another spawner with its partial output intact.
