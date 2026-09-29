use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

pub type AgentId = Uuid;

/// A participant that can send or receive messages.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Addr {
    /// A human principal.
    User(String),
    /// A program principal (e.g. an MCP client such as another agent).
    Client(String),
    Agent(AgentId),
    /// A long-lived named agent from the cluster file; resolved by the hub.
    Resident(String),
    /// A durable queue agents read with `mailbox_take`.
    Mailbox(String),
}

impl Addr {
    /// The built-in bootstrap admin.
    pub fn root() -> Self {
        Addr::User("root".into())
    }

    pub fn user(name: &str) -> Self {
        Addr::User(name.into())
    }
}

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Addr::User(s) => write!(f, "user:{s}"),
            Addr::Client(s) => write!(f, "client:{s}"),
            Addr::Agent(id) => write!(f, "agent:{id}"),
            Addr::Resident(s) => write!(f, "resident:{s}"),
            Addr::Mailbox(s) => write!(f, "mailbox:{s}"),
        }
    }
}

impl FromStr for Addr {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.split_once(':') {
            Some(("user", n)) if !n.is_empty() => Ok(Addr::User(n.into())),
            Some(("client", n)) if !n.is_empty() => Ok(Addr::Client(n.into())),
            Some(("resident", n)) if !n.is_empty() => Ok(Addr::Resident(n.into())),
            Some(("mailbox", n)) if !n.is_empty() => Ok(Addr::Mailbox(n.into())),
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
            "description": "user:<name>, client:<name>, agent:<id> (or a bare agent id), resident:<name>, mailbox:<name>",
            "examples": ["user:maciej", "client:claude", "resident:concierge", "agent:6f1c0f7e-6b0a-4c55-9c3e-2f1f7b6d8a10"]
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
        for a in [
            Addr::user("m"),
            Addr::Client("s1".into()),
            Addr::Agent(id),
            Addr::Resident("r".into()),
            Addr::Mailbox("q".into()),
        ] {
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
        for s in ["", "user", "user:", "client:", "agent:nope", "foo:bar", "nobody", "mailbox:"] {
            assert!(s.parse::<Addr>().is_err(), "{s}");
        }
    }
}
