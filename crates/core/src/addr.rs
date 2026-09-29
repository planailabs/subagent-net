use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

pub type AgentId = Uuid;

/// A participant that can send or receive messages.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Addr {
    User,
    /// An external MCP client session.
    Client(String),
    Agent(AgentId),
}

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Addr::User => f.write_str("user"),
            Addr::Client(s) => write!(f, "client:{s}"),
            Addr::Agent(id) => write!(f, "agent:{id}"),
        }
    }
}

impl FromStr for Addr {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.split_once(':') {
            None if s == "user" => Ok(Addr::User),
            Some(("client", c)) if !c.is_empty() => Ok(Addr::Client(c.into())),
            Some(("agent", id)) => id.parse().map(Addr::Agent).map_err(|e| format!("bad agent id {id:?}: {e}")),
            // A bare uuid is an agent: that is what models tend to pass around.
            None => s.parse().map(Addr::Agent).map_err(|_| format!("bad address {s:?}")),
            _ => Err(format!("bad address {s:?}")),
        }
    }
}

impl Serialize for Addr {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl schemars::JsonSchema for Addr {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Addr".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "user, client:<name>, agent:<id> (or a bare agent id)",
            "examples": ["user", "client:claude", "agent:6f1c0f7e-6b0a-4c55-9c3e-2f1f7b6d8a10"]
        })
    }
}

impl<'de> Deserialize<'de> for Addr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        let id = Uuid::new_v4();
        for a in [Addr::User, Addr::Client("s1".into()), Addr::Agent(id)] {
            assert_eq!(a.to_string().parse::<Addr>().unwrap(), a);
            let j = serde_json::to_string(&a).unwrap();
            assert_eq!(serde_json::from_str::<Addr>(&j).unwrap(), a);
        }
    }

    #[test]
    fn bare_uuid_is_agent() {
        let id = Uuid::new_v4();
        assert_eq!(id.to_string().parse::<Addr>().unwrap(), Addr::Agent(id));
    }

    #[test]
    fn rejects_garbage() {
        for s in ["", "client:", "agent:nope", "foo:bar", "nobody"] {
            assert!(s.parse::<Addr>().is_err(), "{s}");
        }
    }
}
