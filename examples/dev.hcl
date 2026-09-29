# A one-machine cluster: subnet dev examples/dev.hcl
node "local" { capacity = 8 }

agent "lead" {
  description = "Breaks a task down and delegates to workers."
  credential {
    env      = "OPENAI_API_KEY"
    base_url = "https://api.openai.com/v1"
  }
  model         = "gpt-5"
  system_prompt = "You are a lead engineer. Split work into focused subtasks, spawn workers for them, wait for their reports and combine the results."
  nodes         = ["local"]
  spawns        = ["worker"]
  budget        = { max_tokens = 2000000, max_depth = 2, max_children = 8 }
}

agent "worker" {
  description = "Does one focused task."
  credential {
    env      = "OPENAI_API_KEY"
    base_url = "https://api.openai.com/v1"
  }
  model         = "gpt-5-mini"
  system_prompt = "You are a careful engineer. Do the task you are given and answer with the result."
  nodes         = ["local"]
  budget        = { max_tokens = 300000 }
}
