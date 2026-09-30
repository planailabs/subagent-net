//! Accounts: a JSON file of names and argon2 hashes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

pub struct Users {
    path: PathBuf,
    hashes: Mutex<BTreeMap<String, String>>,
}

/// Lowercase letters, digits, `-` and `_`, up to 32: names also appear in
/// memory subjects (`person:<name>`).
pub fn valid_name(name: &str) -> Result<String, String> {
    let n = name.trim().to_lowercase();
    if n.is_empty() || n.len() > 32 || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') || n == "vesper" {
        return Err(format!("{name:?}: names are 1-32 of a-z, 0-9, - and _ (and not vesper)"));
    }
    Ok(n)
}

impl Users {
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let hashes = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Users { path, hashes: Mutex::new(hashes) })
    }

    /// Adds a user or changes their password.
    pub fn set(&self, name: &str, password: &str) -> anyhow::Result<String> {
        let name = valid_name(name).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(password.chars().count() >= 6, "the password needs at least 6 characters");
        let hash = Argon2::default()
            // A v4 UUID is 122 random bits from the OS: plenty for a salt.
            .hash_password(password.as_bytes(), &SaltString::encode_b64(uuid::Uuid::new_v4().as_bytes()).map_err(|e| anyhow::anyhow!("salt: {e}"))?)
            .map_err(|e| anyhow::anyhow!("hashing: {e}"))?
            .to_string();
        let mut h = self.hashes.lock().unwrap();
        h.insert(name.clone(), hash);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&*h)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(name)
    }

    /// The canonical name if the password is right.
    pub fn check(&self, name: &str, password: &str) -> Option<String> {
        let name = valid_name(name).ok()?;
        let hash = self.hashes.lock().unwrap().get(&name)?.clone();
        let parsed = PasswordHash::new(&hash).ok()?;
        Argon2::default().verify_password(password.as_bytes(), &parsed).ok().map(|_| name)
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.lock().unwrap().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_check_and_persist() {
        let dir = std::env::temp_dir().join(format!("vesper-users-{}", std::process::id()));
        let path = dir.join("users.json");
        let _ = std::fs::remove_file(&path);
        let u = Users::open(path.clone()).unwrap();
        assert!(u.is_empty());
        assert_eq!(u.set(" Alice ", "secret1").unwrap(), "alice");
        assert!(u.set("bob", "short").is_err());
        assert!(u.set("bo b", "longenough").is_err());
        assert!(u.set("vesper", "longenough").is_err());
        assert_eq!(u.check("ALICE", "secret1").as_deref(), Some("alice"));
        assert!(u.check("alice", "wrong").is_none());
        assert!(u.check("nobody", "secret1").is_none());
        let again = Users::open(path).unwrap();
        assert_eq!(again.check("alice", "secret1").as_deref(), Some("alice"));
        assert!(!std::fs::read_to_string(dir.join("users.json")).unwrap().contains("secret1"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
