//! Spawner config (TOML) and agent type identity.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subnet_core::agent::Budget;
use subnet_core::proto::TypeInfo;
use subnet_llm::ModelConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Hub WebSocket URL, e.g. `ws://127.0.0.1:7700/spawner`.
    pub hub: String,
    pub name: String,
    #[serde(default = "default_capacity")]
    pub capacity: u32,
    /// Env var holding the hub token.
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(rename = "type", default)]
    pub types: Vec<TypeConfig>,
}

fn default_capacity() -> u32 {
    16
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypeConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub system: String,
    pub model: ModelConfig,
    #[serde(default)]
    pub mcp: Vec<McpServerConfig>,
    #[serde(default)]
    pub spawns: Vec<String>,
    #[serde(default)]
    pub budget: Budget,
    /// Tools that need user approval.
    #[serde(default)]
    pub approve: Vec<String>,
    /// Tools safe to re-run after a crash interrupted them.
    #[serde(default)]
    pub idempotent: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    /// Stdio server: command to launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment for the stdio server. Values name env vars on the
    /// spawner (`KEY = "$VAR"`) or are literals.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Streamable-HTTP server URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Self::parse(&s)
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let c: Config = toml::from_str(s)?;
        let mut seen = std::collections::HashSet::new();
        for t in &c.types {
            anyhow::ensure!(seen.insert(&t.name), "duplicate type {:?}", t.name);
            anyhow::ensure!(!t.name.contains('@'), "type name {:?} may not contain '@'", t.name);
            for m in &t.mcp {
                anyhow::ensure!(
                    m.command.is_some() != m.url.is_some(),
                    "mcp server {:?} needs exactly one of command or url",
                    m.name
                );
            }
        }
        Ok(c)
    }
}

impl TypeConfig {
    /// Stable hash of everything that defines the type's behaviour. Secrets are
    /// only referenced by env var name, so they don't leak into it.
    pub fn hash(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("type config serializes");
        Sha256::digest(&bytes).iter().take(6).map(|b| format!("{b:02x}")).collect()
    }

    pub fn info(&self) -> TypeInfo {
        TypeInfo {
            name: self.name.clone(),
            hash: self.hash(),
            description: self.description.clone(),
            spawns: self.spawns.clone(),
            budget: self.budget.clone(),
            approve: self.approve.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
hub = "ws://127.0.0.1:7700/spawner"
name = "laptop"
capacity = 4

[[type]]
name = "coder"
system = "You write Rust."
model = { base_url = "http://localhost:8000/v1", model = "qwen", prefill = true, params = { temperature = 0.2 } }
spawns = ["reviewer"]
budget = { max_tokens = 100000, max_depth = 2, max_children = 4 }
approve = ["write_file"]
idempotent = ["read_file"]
mcp = [{ name = "fs", command = "mcp-fs", args = ["--root", "."] }]

[[type]]
name = "reviewer"
system = "You review."
model = { base_url = "https://api.openai.com/v1", model = "gpt-5", api_key_env = "OPENAI_API_KEY" }
"#;

    #[test]
    fn parses_example() {
        let c = Config::parse(EXAMPLE).unwrap();
        assert_eq!(c.capacity, 4);
        assert_eq!(c.types.len(), 2);
        let coder = &c.types[0];
        assert!(coder.model.prefill);
        assert_eq!(coder.model.params["temperature"], 0.2);
        assert_eq!(coder.budget.max_tokens, Some(100000));
        assert_eq!(coder.mcp[0].args, ["--root", "."]);
        assert_eq!(c.types[1].budget, Budget::default());
    }

    #[test]
    fn hash_is_stable_and_sensitive() {
        let c = Config::parse(EXAMPLE).unwrap();
        let t = &c.types[0];
        assert_eq!(t.hash(), t.clone().hash());
        assert_eq!(t.hash().len(), 12);
        let mut t2 = t.clone();
        t2.system.push('!');
        assert_ne!(t.hash(), t2.hash());
        assert_eq!(t.info().id(), format!("coder@{}", t.hash()));
    }

    #[test]
    fn rejects_bad_configs() {
        let dup = format!(
            "{EXAMPLE}\n[[type]]\nname = \"coder\"\nsystem = \"\"\nmodel = {{ base_url = \"x\", model = \"y\" }}\n"
        );
        assert!(Config::parse(&dup).unwrap_err().to_string().contains("duplicate"));
        let both = EXAMPLE.replace(r#"command = "mcp-fs""#, r#"command = "mcp-fs", url = "http://x""#);
        assert!(Config::parse(&both).is_err());
        let at = EXAMPLE.replace(r#"name = "reviewer""#, r#"name = "re@viewer""#);
        assert!(Config::parse(&at).is_err());
    }
}
