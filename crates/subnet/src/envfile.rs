//! `.env` files for bring-up: `--env-file PATH` (repeatable) and `./.env`,
//! loaded before the CLI is parsed (flags read env defaults) and before any
//! thread starts. A variable that is already set is never overwritten, so
//! the real environment wins, then earlier files over later ones.

use std::path::PathBuf;

/// The `--env-file` values in `args`.
pub fn requested(args: &[String]) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            break;
        }
        if a == "--env-file" {
            if let Some(p) = it.next() {
                out.push(PathBuf::from(p));
            }
        } else if let Some(p) = a.strip_prefix("--env-file=") {
            out.push(PathBuf::from(p));
        }
    }
    out
}

/// Loads the requested files (each must exist), then `./.env` if present.
/// Returns the files loaded. Call before starting threads.
pub fn load(args: &[String]) -> anyhow::Result<Vec<PathBuf>> {
    let mut loaded = vec![];
    for p in requested(args) {
        dotenvy::from_path(&p).map_err(|e| anyhow::anyhow!("--env-file {}: {e}", p.display()))?;
        loaded.push(p);
    }
    match dotenvy::from_path(".env") {
        Ok(()) => loaded.push(".env".into()),
        Err(e) if e.not_found() => {}
        Err(e) => anyhow::bail!(".env: {e}"),
    }
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn finds_env_file_flags() {
        assert_eq!(requested(&args(&["subnet", "--env-file", "a.env", "dev", "--env-file=b.env", "x.hcl"])), [PathBuf::from("a.env"), "b.env".into()]);
        assert!(requested(&args(&["subnet", "send", "--", "--env-file", "no"])).is_empty());
        assert!(requested(&args(&["subnet", "--env-file"])).is_empty());
    }

    #[test]
    fn earlier_sources_win() {
        let dir = std::env::temp_dir().join(format!("subnet-envfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.env"), dir.join("b.env"));
        std::fs::write(&a, "SUBNET_ENVFILE_T1=from-a\n# comment\nSUBNET_ENVFILE_T2=\"quoted a\"\n").unwrap();
        std::fs::write(&b, "SUBNET_ENVFILE_T1=from-b\nSUBNET_ENVFILE_T3=from-b\n").unwrap();
        let loaded = load(&args(&["x", "--env-file", a.to_str().unwrap(), "--env-file", b.to_str().unwrap()])).unwrap();
        assert_eq!(&loaded[..2], [a.clone(), b.clone()]);
        assert_eq!(std::env::var("SUBNET_ENVFILE_T1").unwrap(), "from-a");
        assert_eq!(std::env::var("SUBNET_ENVFILE_T2").unwrap(), "quoted a");
        assert_eq!(std::env::var("SUBNET_ENVFILE_T3").unwrap(), "from-b");
        assert!(load(&args(&["x", "--env-file", dir.join("missing.env").to_str().unwrap()])).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
