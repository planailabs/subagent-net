# The cluster example from DESIGN.md. Apply with: subnet apply examples/cluster.hcl
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
