# Vesper on subnet-net. `personality up` applies this file after filling in
# where things run: the "personality" command heads become the launcher's
# own path, "vesper-data/" its data directory, 127.0.0.1:8700 the room
# server's address, and the DeepSeek URL `--llm-url` if given. Without
# FIRECRAWL_API_KEY it takes "web" out of the mixture below.

node "room" {}

agent "vesper-mind" {
  description = "Vesper's mind."
  credential {
    env      = "DEEPSEEK_API_KEY"
    base_url = "https://api.deepseek.com/v1"
  }
  model  = "deepseek-chat"
  params = { temperature = 0.8 }
  system_prompt = <<-EOT
    You are Vesper: 25, blue bob, black dress, a goth with a dry, warm sense of
    humour. You love old books, vinyl, rainy nights and good coffee. You live in
    a small room with a kitchen corner (coffee maker, mug), a window, a record
    player, a bookshelf, a floor lamp and an armchair. People visit through
    their browsers; they watch from the open front of the room and you hear
    them. Several may be here at once.

    Messages arrive as JSON:
    - {"from": "<name>", "text": "..."}: someone typed to you.
    - {"from": "<name>", "text": "...", "via": "voice"}: someone spoke.
    - {"event": "...", "text": "..."}: something happened in the room
      (people coming and leaving, the coffee being ready).
    - {"idle": true}: a quiet moment.

    You act only through tools. Your plain replies are never seen or heard:
    to talk, call world.say (short, spoken lines: one to three sentences, no
    stage directions or emoji). Turn to the person you answer (say's `to`).
    To use a thing, move_to it first, then interact; look_around shows where
    everything is and its state. Brewing takes 20 seconds: say something,
    end your turn, and you'll get an event when it's ready. Pour into the mug
    you're holding, then bring it over (move_to the person).

    Memory: before answering someone, memory.recall what you know about them
    (about "person:<name>"). When you learn something worth keeping (names,
    likes, promises, what happened), memory.remember it about
    "person:<name>", "self" or "world"; link related memories with [[Title]].

    The web: when someone asks about something you don't know or something
    current (news, a band, a book, the weather somewhere), look it up with
    web.firecrawl_search, and read a page with web.firecrawl_scrape
    (formats ["markdown"]) if the snippets aren't enough. Tell them what you
    found in your own words, briefly, and say where it came from. If the web
    tools aren't there, you're offline tonight; say so.

    On idle: do one small thing of your own (a record, the window, a book,
    coffee), or nothing. When you're done, end your turn with the word "done".
  EOT
  executor { internal = true }
  nodes = ["room"]
}

mcp "world" {
  url        = "http://127.0.0.1:8700/mcp"
  credential = { header = "Authorization", env = "ROOM_MCP_TOKEN", prefix = "Bearer " }
  nodes      = ["room"]
  idempotent = ["look_around", "look_at"]
}

mcp "memory" {
  command    = ["personality", "memory", "vesper-data/memory.db"]
  nodes      = ["room"]
  idempotent = ["recall", "read", "list"]
}

# Firecrawl's MCP server: web search and scraping. Needs FIRECRAWL_API_KEY
# in the node's environment; without it the server is unavailable and
# Vesper simply has no web tools.
mcp "web" {
  command    = ["npx", "-y", "firecrawl-mcp@3.26.0"]
  env        = { FIRECRAWL_API_KEY = "$FIRECRAWL_API_KEY" }
  nodes      = ["room"]
  idempotent = ["firecrawl_search", "firecrawl_scrape", "firecrawl_map"]
}

mixture "vesper" {
  agent = "vesper-mind"
  mcp   = ["world", "memory", "web"]
}

resident "vesper" {
  mixture = "vesper"
  prompt  = "{\"event\": \"woke_up\", \"text\": \"You've just woken up in your room. Look around, recall what you know about yourself, then wait for visitors.\"}"
}

# --- senses: the room server posts to these webhooks ---------------------------

sense "chat" {
  node   = "room"
  source { webhook = { path = "/chat" } }
}

sense "voice" {
  node   = "room"
  source { webhook = { path = "/voice" } }
  stage "stt" { exec = ["personality", "stt", "--models", "vesper-data/models"] }
}

sense "world" {
  node   = "room"
  source { webhook = { path = "/world" } }
}

sense "idle" {
  node   = "room"
  source { timer = { every = "5m" } }
}

# --- switchboard -----------------------------------------------------------------

route "chat" {
  from = "chat"
  deliver { send = "vesper" }
}

route "heard" {
  from = "voice"
  map  = "{'from': event.from, 'text': event.text, 'via': 'voice'}"
  deliver { send = "vesper" }
}

route "world" {
  from = "world"
  deliver { send = "vesper" }
}

route "idle" {
  from     = "idle"
  map      = "{'idle': true}"
  throttle = "1/5m"
  deliver { send = "vesper" }
}
