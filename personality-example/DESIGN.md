# personality-example design

A virtual personality living in a small 3D room, built on subagent-net. **Vesper** — about 25, blue hair, goth-ish — is a resident agent. People log in from a browser, see the room, and talk to her by chat or microphone. She answers with her voice, walks around, and uses the things in the room (a coffee maker among them). She remembers herself, each person she meets, and the world.

> Kept in sync with the code, like the top-level DESIGN.md. The Status section says what is implemented.

## Pieces

```
browsers: login · three.js room · chat · push-to-talk mic · her voice
    │ WebSocket (state, chat, speech)          ▲
    ▼                                          │
room server (Rust, crates/room) ───────────────┘
  • world simulation: positions, pathfinding, object states, her animation state
  • users (argon2) and sessions; presence
  • MCP "world" (streamable HTTP /mcp): say, move_to, look_at, gesture, interact, look_around
  • Piper TTS for `say`; speech audio + a lip-sync envelope go to every browser
  • webhooks into the subnet node: chat, voice (utterances as WAV blobs), world events
    │
    ▼
subnet node "room" (senses) ──► hub switchboard ──► resident "vesper"
  chat ────────────────────────── route "chat"  ──► send
  voice ── stage stt (whisper) ── route "heard" ──► send
  world ───────────────────────── route "world" ──► send
  idle (timer) ────────────────── route "idle"  ──► send (only after silence)
                                                        │ tool calls (MCP via the hub)
memory MCP (Rust, crates/memory, stdio) ◄──────────────┤
  SQLite notes + [[wikilinks]] + FTS5 (BM25) + e5 embeddings (fastembed)
  subjects: self · person:<name> · world                 │
world MCP (the room server) ◄───────────────────────────┘
```

Vesper is the resident `vesper` of mixture `vesper` (agent type `vesper-mind` on DeepSeek, MCP servers `world`, `memory` and `web`), declared in [cluster.hcl](cluster.hcl). The resident's first message is a `woke_up` event.

## One shared room

There is one world and one Vesper. Everyone logged in sees the same room and hears the same voice. Every message she gets says who it's from (`{"from": "alice", "text": "…"}`), so she can address people and remember them separately.

## How she acts: everything is a tool

Her plain answers are never shown. She acts only through tools, so every action is explicit, logged in her event log, and stops when she's paused:

| tool | effect |
|---|---|
| `world.say(text, to?)` | Piper speaks; every browser plays it, her mouth follows the loudness, the text appears in the chat. Returns the duration. |
| `world.move_to(target)` | walks to an object or `{x, z}` along a path around furniture; returns on arrival. |
| `world.look_at(target)` | turns to a person (by name), an object or a point. |
| `world.gesture(name)` | `wave`, `nod`, `shrug`, `think`. |
| `world.interact(object, action)` | uses a thing; she must stand next to it (≤ 1.2 m). |
| `world.look_around()` | where she is, who's in the room, every object with its state, distance and actions. |
| `memory.remember(title, body, about)` | writes a memory about `self`, `person:<name>` or `world`; `[[links]]` connect memories. |
| `memory.recall(query, about?, k?)` | hybrid search: BM25 and embedding similarity. |
| `memory.read(title)` / `memory.forget(title)` / `memory.list(about?)` | |
| `web.firecrawl_search(query)`, `web.firecrawl_scrape(url)`, … | Firecrawl's MCP server (`npx firecrawl-mcp@3.26.0`, stdio): she looks things up when asked about something she doesn't know or something current, and says where it came from. |

## The room and its things

A 8 m × 6 m room: kitchen corner, reading corner, a window.

| object | actions | state |
|---|---|---|
| `coffee_maker` | `brew` (20 s; a world event says when it's done), `pour` | idle / brewing / ready |
| `mug` | `pick_up`, `drink`, `put_down` | where it is; empty / coffee |
| `lamp` | `toggle` | on / off (lights the reading corner in browsers) |
| `record_player` | `play`, `stop` | playing / stopped, track |
| `armchair` | `sit`, `stand` | occupied |
| `bookshelf` | `browse` (returns titles) | |
| `window` | `look_outside` (time of day from the clock) | |

Rules live in the pure `world` module of the room server: distances, what needs what (`pour` needs `ready` coffee and the mug in hand, `drink` needs coffee in the mug), and what each action changes. Things that happen later (coffee ready) are **world events**: the room server posts them to the `world` webhook and they reach Vesper through the switchboard.

Walking: a 0.25 m grid over the floor, cells within her radius (0.25 m) of walls and furniture blocked, shortest grid path (Dijkstra to the nearest cell within 0.9 m of a thing), straightened where she can see past waypoints, 1.2 m/s. Sent to a thing, she ends up facing it. Walking off the armchair stands her up.

A counter (kitchen) and a side table (reading corner) are furniture: in the way, and surfaces. `put_down` puts the mug on the nearest surface in reach, else on the floor. Looking at a person turns her to the viewers at the open front.

## The character and objects

Modelled in Blender by Python scripts in [blender/](blender/): `python3 blender/live.py blender/build.py assets [--preview DIR]` runs them in the open Blender through the Blender MCP add-on (port 9876) and renders previews; `blender --background --factory-startup --python blender/build.py -- assets` is the reproducible build. They build into their own scenes (`Vesper`, `Room`) and export, all committed:

- `assets/vesper.glb`: one skinned mesh `vesper`, the armature, the clips and the shape key.
- `assets/props.glb`: one top-level node per world object id, origin at the centre of its footprint on the surface it stands on, front towards +z. Parts the browser changes are child nodes: `pot`, `coffee`, `led` (coffee maker), `mug_fill`, `platter`, `shade`, `flame` (the candle), `glass`.
- `assets/room.glb`: floor, walls with the window opening, rug, a picture, and `sky` behind the window (tinted by time of day).

- **Vesper:** sculpted, not assembled from primitives. The body is one continuous mesh grown from a skin-modifier skeleton, subdivided, shaped with sculpt-style brushes (bust, hips, waist, calves, collarbones) and heat-weighted to the rig. The head is a dense mesh sculpted the same way (jaw, chin, cheekbones, eye sockets, nose, lips), with textured eyes, winged liner and brows. The mouth is cut along a straight line so it can open onto a dark mouth. She wears a blue bob with blunt bangs and strand grooves (a solidified shell), an off-shoulder black dress with a sweetheart neckline and long sleeves with flared cuffs (shells lifted off the body), a folded skirt with a violet hem and sash (following the thighs lower down), fishnet tights, platform boots with a buckle strap, and a choker. The armature has clips `idle`, `walk`, `talk`, `wave`, `reach`, `sit`, `think`, `nod`, `shrug`. A `mouth_open` shape key drops the jaw about a hinge and follows her speech loudness. The brushes are plain functions in `blender/vesper.py`, so the sculpt rebuilds identically.
- **Objects:** coffee maker (with a pot), mug, floor lamp, record player, armchair, bookshelf with books, window, plus the room shell.

## The web

The `web` MCP server is Firecrawl's own (`firecrawl-mcp` from npm, pinned), started by the node with `FIRECRAWL_API_KEY` from its environment; `FIRECRAWL_API_URL` (inherited) points it at a self-hosted Firecrawl. A spawn needs every MCP server of its mixture live, so without a key the launcher takes `web` out of the mixture and warns: Vesper runs, just without the web.

## Voice in and out

- **Out:** `say` runs Piper (`piper` from nixpkgs; the voice, `en_GB-jenny_dioco-medium` by default, is downloaded from Hugging Face into `<data>/voices` once) to WAV, computes a loudness envelope (RMS per 40 ms, normalised), and sends both to all browsers; the tool returns when she's done speaking. Without Piper on PATH (`--tts auto`), or with `--tts silent` (tests), speech is silent, 0.3 s per word, and her mouth still moves.
- **In:** push-to-talk in the browser records 16 kHz mono PCM. The room server wraps each utterance as a WAV blob and posts `{from, audio: {"$blob": …}}` to the `voice` webhook. The `stt` stage (`vesper-stt`, crates/stt: whisper.cpp via whisper-rs, `ggml-base.en` downloaded into `vesper-data/models` once) gets each event as a JSON line and turns it into `{from, text}`. It drops the audio, so it never reaches the blob store; silence and annotations like `[BLANK_AUDIO]` give no event; `--fake TEXT` hears the same words every time (tests). Route `heard` sends it to Vesper. The speaker's identity travels with the utterance.

## Memory

A Rust port of self-learning-model's store. It's one SQLite file:

- `memories(slug, title, body, about, created, updated, embedding)`
- `links(from, to)`, from `[[wikilinks]]` in bodies
- an FTS5 index (title weighted ×5)

`recall` merges BM25 hits with cosine-similar embeddings (multilingual e5-small via fastembed, downloaded on first use; `SUBJECT_MEMORY_EMBEDDINGS=off` for BM25 only) and returns linked titles too.

**Subjects:**

- `self`: her own history and preferences
- `person:<name>`: one per user she's met
- `world`: the room, its things, what happened

The system prompt makes her recall about a speaker before answering, and write down what matters.

## Users

The room server keeps users in `<data>/users.json` (`vesper-room adduser <name>` prompts for a password; argon2), an HttpOnly session cookie, and presence (a person is in the room while they have a connection open; coming and leaving are world events). It needs no hub principal: it only posts to the node's webhooks, and speakers are named inside the messages. The world MCP takes a bearer token (`ROOM_MCP_TOKEN`), which the cluster's `mcp "world"` block sends as a credential.

### Room server interface

- `POST /api/login {name, password}` sets the cookie; `POST /api/logout`; `GET /api/me`.
- `GET /ws` (cookie): the server sends `hello {you, chat}` (recent chat), `state {t, vesper, objects, people}` every 100 ms, `chat {from, text, to?, voice?}` (from `null` for arrivals), and `speech {id, url, secs, frame, envelope, text}`. The browser sends `chat {text}`, and push-to-talk as `voice_start`, binary 16 kHz mono s16le frames, `voice_end` (or `voice_cancel`). Utterances under 0.25 s are dropped; the limit is 30 s.
- `GET /speech/<id>.wav` (cookie): the last 32 speeches.
- `/mcp`: the world MCP. `/assets/*`: the .glb files. Everything else: the web app.
- Webhooks out: `<hooks>/chat {from, text}`, `<hooks>/voice {from, audio: {$blob}}`, `<hooks>/world {event: joined|left|coffee_ready, who?, text}`.

## Running

`nix develop` builds everything (Rust, Postgres, node, cmake and libclang for whisper.cpp); `nix develop .#personality` adds Piper, and `nix develop .#assets` has Blender for the headless asset build. See [README.md](README.md).

`personality` (crates/personality) is one binary:

- `personality up` starts, in one process: a Postgres (private, in `vesper-data/pg`, stopped on Ctrl-C; or `--database`/`$DATABASE_URL`), the subnet hub (`127.0.0.1:8780`, with a fresh admin token it prints), the room server (`127.0.0.1:8700`), the cluster applied, the node `room` in-process and its webhooks (`127.0.0.1:8790`). It sets `ROOM_MCP_TOKEN` to a random token unless given. `DEEPSEEK_API_KEY` is needed for Vesper to think; `--llm-url` uses another OpenAI-compatible endpoint; `--silent` skips Piper.
- `personality adduser <name>` adds a login.
- `personality memory <db>` and `personality stt …` are the memory MCP server and the stt stage the cluster file starts. (`vesper-memory` and `vesper-stt` are the same as standalone binaries.)

Before applying [cluster.hcl](cluster.hcl), the launcher fills in this run's details: command heads `"personality"` become its own path, `vesper-data/` its data directory, `127.0.0.1:8700` the room's address, and the DeepSeek URL `--llm-url`.

## Tests

- memory: storage, links, BM25, subjects, forgetting (dense retrieval test ignored unless the model is cached)
- world: pathfinding, interaction rules, the coffee timeline
- room server: login, WebSocket state, chat → webhook, the MCP tools
- stt stage: the protocol with a fake transcriber (real whisper test ignored by default)
- assets: every .glb loads, has the expected nodes and animation clips
- end to end (`crates/personality/tests/e2e.rs`): private Postgres, hub, node, room server and the real cluster file, with a scripted OpenAI-compatible brain. Alice says "make me a coffee" into her microphone (stt in fake mode); Vesper recalls, walks to the coffee maker, picks up the mug, brews, says so, is told by a world event that it's ready, pours, brings it to the front, says so (Alice's browser gets both speeches), and remembers who asked
- web: pure logic (lip-sync, state interpolation) with `node --test`

## Status

Implemented:

- memory: `vesper-memory` stdio MCP server (crates/memory) with tests
- world simulation: `vesper_room::world` (pure) with tests
- assets: Blender scripts and the three .glb files, checked by `crates/room/tests/assets.rs`
- room server: `vesper-room` (login, WebSocket, world MCP, Piper/silent TTS, webhooks) with tests
- stt stage: `vesper-stt` with tests (a real Piper → whisper round trip is `--ignored`)
- launcher and end-to-end test: `personality`, `cluster.hcl`
- web: Firecrawl MCP; `tests/web.rs` (`--ignored`: npx) runs the real `firecrawl-mcp` against a stand-in Firecrawl API, and Vesper answers from the search
- web client (`web/`, Vue + Parcel + three.js): login, the 3D room following the state, clip blending (the pose's legs under an action's upper body), lip sync from the speech envelope, chat, push-to-talk (button or space; resampled to 16 kHz in the browser); `npm test` covers the pure logic

Everything described here is implemented.
