# Vesper: a virtual personality on subagent-net

Vesper lives in a small 3D room. She's a resident agent of the subnet hub,
and her mind is DeepSeek. People log in from a browser, watch the room, and
talk to her by chat or by holding the talk button. She answers with Piper's
voice and lip sync, walks around, makes coffee, plays records, and remembers
herself, everyone she meets, and the room.

[DESIGN.md](DESIGN.md) has the architecture.

## Run

```sh
nix develop .#personality                         # Rust, Postgres, node, Piper
(cd personality-example/web && npm install && npm run build)
cargo run -p personality -- adduser alice         # asks for a password
DEEPSEEK_API_KEY=… cargo run -p personality -- up
```

Open the room at http://127.0.0.1:8700. The subnet hub UI at
http://127.0.0.1:8780 (use the admin token it prints) shows her thinking,
her tool calls and the switchboard.

- The first run downloads a Piper voice, the whisper model (`base.en`) and
  the e5 embedding model into `vesper-data/`.
- `--silent` skips Piper (her mouth still moves). `--llm-url` points her
  mind at any OpenAI-compatible endpoint. `--database` (or `DATABASE_URL`)
  uses your Postgres instead of a private one in `vesper-data/pg`.

## Develop

| part | where | test |
|---|---|---|
| memory MCP (SQLite, BM25 + e5) | `crates/memory` | `cargo test -p vesper-memory` |
| world simulation, room server, world MCP, TTS | `crates/room` | `cargo test -p vesper-room` |
| speech-to-text stage | `crates/stt` | `cargo test -p vesper-stt` |
| launcher, end to end | `crates/personality` | `cargo test -p personality` |
| web client (Vue, three.js) | `web/` | `npm test`; `npm run dev` proxies to a running room |
| Blender assets | `blender/`, `assets/` | tested by `vesper-room`'s `assets` test |

Tests that need downloads are `#[ignore]`d: real e5 recall, Piper, and a
Piper → whisper round trip (`cargo test -p vesper-stt -- --ignored` inside
`nix develop .#personality`).

Rebuild the assets in the open Blender (with the Blender MCP add-on running)
and render previews:

```sh
python3 personality-example/blender/live.py personality-example/blender/build.py personality-example/assets --preview /tmp/previews
```

or headless: `nix develop .#assets -c blender --background --factory-startup --python personality-example/blender/build.py -- personality-example/assets`.
