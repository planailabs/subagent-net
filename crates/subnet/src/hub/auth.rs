//! Cluster versions, principals and tokens.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subnet_cluster::{Change, Cluster};
use subnet_core::addr::Addr;
use subnet_ops::Role;

use super::db::{ClusterFile, VersionInfo};
use super::{Hub, HubError, ct_eq};
use crate::api::Principal;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalKind {
    User,
    Client,
    Node,
}

impl PrincipalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PrincipalKind::User => "user",
            PrincipalKind::Client => "client",
            PrincipalKind::Node => "node",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(PrincipalKind::User),
            "client" => Some(PrincipalKind::Client),
            "node" => Some(PrincipalKind::Node),
            _ => None,
        }
    }
}

/// The applied cluster.
#[derive(Debug, Clone, Default)]
pub struct ClusterState {
    pub version: Option<VersionInfo>,
    pub files: Vec<ClusterFile>,
    pub spec: Cluster,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Applied {
    /// The new version; none for a dry run or when nothing changed.
    pub version: Option<i64>,
    pub changes: Vec<Change>,
}

pub fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn role(r: subnet_cluster::Role) -> Role {
    match r {
        subnet_cluster::Role::Viewer => Role::Viewer,
        subnet_cluster::Role::Operator => Role::Operator,
        subnet_cluster::Role::Admin => Role::Admin,
    }
}

impl Hub {
    pub(crate) async fn load_auth(&self) -> Result<(), HubError> {
        *self.cluster.write().unwrap() = ClusterState::default();
        self.tokens.write().unwrap().clear();
        if let Some((v, files)) = self.db.latest_cluster().await? {
            let texts: Vec<(&str, &str)> = files.iter().map(|f| (f.name.as_str(), f.text.as_str())).collect();
            // It validated when applied; a failure now means the parser got stricter.
            let spec = Cluster::parse(&texts).map_err(|e| HubError::Bad(format!("stored cluster version {}: {e}", v.version)))?;
            let routes = subnet_switchboard::compile(&spec).map_err(HubError::Bad)?;
            *self.cluster.write().unwrap() = ClusterState { version: Some(v), files, spec };
            self.set_routes(routes);
        }
        let rows = self.db.tokens().await?;
        let mut t = self.tokens.write().unwrap();
        for (hash, kind, name) in rows {
            if let Some(k) = PrincipalKind::parse(&kind) {
                t.insert(hash, (k, name));
            }
        }
        Ok(())
    }

    pub fn cluster(&self) -> ClusterState {
        self.cluster.read().unwrap().clone()
    }

    /// Open mode: no admin token configured. Everyone is `user:root`.
    pub fn open_mode(&self) -> bool {
        self.admin_token.is_none()
    }

    /// Resolves a bearer token to a principal.
    pub fn authenticate(&self, bearer: Option<&str>) -> Result<Principal, HubError> {
        let root = || Principal { addr: Addr::root(), role: Role::Admin };
        if let (Some(admin), Some(b)) = (&self.admin_token, bearer)
            && ct_eq(admin.as_bytes(), b.as_bytes())
        {
            return Ok(root());
        }
        if let Some(b) = bearer
            && let Some((kind, name)) = self.tokens.read().unwrap().get(&token_hash(b)).cloned()
        {
            let c = self.cluster.read().unwrap();
            let def = match kind {
                PrincipalKind::User => c.spec.users.get(&name),
                PrincipalKind::Client => c.spec.clients.get(&name),
                PrincipalKind::Node => return Err(HubError::Unauthorized("node tokens only work for nodes".into())),
            };
            let Some(def) = def else {
                return Err(HubError::Unauthorized(format!("{} {name:?} is no longer declared", kind.as_str())));
            };
            let addr = if kind == PrincipalKind::User { Addr::User(name) } else { Addr::Client(name) };
            return Ok(Principal { addr, role: role(def.role) });
        }
        if self.open_mode() {
            return Ok(root());
        }
        Err(HubError::Unauthorized("missing or unknown token".into()))
    }

    /// May a node connect under this name with this token?
    pub fn node_ok(&self, name: &str, token: Option<&str>) -> bool {
        if self.open_mode() {
            return true;
        }
        let Some(t) = token else { return false };
        if ct_eq(self.admin_token.as_deref().unwrap_or_default().as_bytes(), t.as_bytes()) {
            return true;
        }
        let declared = self.cluster.read().unwrap().spec.nodes.contains_key(name);
        declared
            && self
                .tokens
                .read()
                .unwrap()
                .get(&token_hash(t))
                .is_some_and(|(k, n)| *k == PrincipalKind::Node && n == name)
    }

    /// Validates and (unless `dry_run`) stores a new cluster version.
    pub async fn apply_cluster(&self, files: Vec<ClusterFile>, dry_run: bool, by: &Addr) -> Result<Applied, HubError> {
        if files.is_empty() {
            return Err(HubError::Bad("no files".into()));
        }
        let texts: Vec<(&str, &str)> = files.iter().map(|f| (f.name.as_str(), f.text.as_str())).collect();
        let spec = Cluster::parse(&texts).map_err(|e| HubError::Bad(e.to_string()))?;
        let routes = subnet_switchboard::compile(&spec).map_err(HubError::Bad)?;
        // Hooks' `when`, like routes' expressions, is checked now.
        for (name, h) in &spec.hooks {
            if let Some(w) = &h.when {
                subnet_switchboard::Expr::compile(w).map_err(|e| HubError::Bad(format!("hook {name:?}: when: {e}")))?;
            }
        }
        let changes = self.cluster.read().unwrap().spec.diff(&spec);
        if dry_run || (changes.is_empty() && self.cluster.read().unwrap().version.is_some()) {
            return Ok(Applied { version: None, changes });
        }
        let v = self.db.add_cluster_version(&files, &by.to_string()).await?;
        let version = v.version;
        *self.cluster.write().unwrap() = ClusterState { version: Some(v), files, spec };
        self.set_routes(routes);
        tracing::info!(version, changes = changes.len(), by = %by, "cluster applied");
        self.reconfigure_nodes().await;
        self.sync_residents().await;
        Ok(Applied { version: Some(version), changes })
    }

    pub async fn rollback_cluster(&self, version: i64, by: &Addr) -> Result<Applied, HubError> {
        let Some((_, files)) = self.db.cluster_version(version).await? else {
            return Err(HubError::NotFound(format!("no cluster version {version}")));
        };
        self.apply_cluster(files, false, by).await
    }

    pub async fn cluster_history(&self) -> Result<Vec<VersionInfo>, HubError> {
        Ok(self.db.cluster_history().await?)
    }

    /// Creates a token for a declared principal. Only its hash is kept.
    pub async fn issue_token(&self, kind: PrincipalKind, name: &str) -> Result<String, HubError> {
        let c = self.cluster.read().unwrap().spec.clone();
        let declared = match kind {
            PrincipalKind::User => c.users.contains_key(name),
            PrincipalKind::Client => c.clients.contains_key(name),
            PrincipalKind::Node => c.nodes.contains_key(name),
        };
        if !declared {
            return Err(HubError::NotFound(format!("{} {name:?} is not declared in the cluster", kind.as_str())));
        }
        let token = format!("snt_{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let hash = token_hash(&token);
        self.db.add_token(&hash, kind.as_str(), name).await?;
        self.tokens.write().unwrap().insert(hash, (kind, name.to_string()));
        Ok(token)
    }

    pub async fn revoke_tokens(&self, kind: PrincipalKind, name: &str) -> Result<u64, HubError> {
        let n = self.db.revoke_tokens(kind.as_str(), name).await?;
        self.tokens.write().unwrap().retain(|_, (k, n2)| !(*k == kind && n2 == name));
        Ok(n)
    }
}
