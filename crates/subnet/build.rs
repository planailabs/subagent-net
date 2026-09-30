// The hub embeds the web UI. Which build it embeds, in order:
// 1. webui/dist, when you've built it (`npm run build` in webui/);
// 2. otherwise a fresh build in cargo's OUT_DIR, when npm is on PATH (a git
//    dependency's checkout has no dist; set SUBNET_WEBUI_BUILD=0 to skip);
// 3. otherwise a page saying the UI isn't built, so the Rust build never
//    depends on node.
// The chosen directory reaches rust-embed as SUBNET_WEBUI_DIST.
// Shelling out: npm/Parcel is the web UI's build tool.

use std::path::{Path, PathBuf};
use std::process::Command;

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), target)?;
        }
    }
    Ok(())
}

/// Builds webui (sources copied into `work`); returns its dist, or why not.
fn npm_build(webui: &Path, work: &Path) -> Result<PathBuf, String> {
    if std::env::var("SUBNET_WEBUI_BUILD").is_ok_and(|v| v == "0") {
        return Err("SUBNET_WEBUI_BUILD=0".into());
    }
    if !webui.join("package.json").exists() {
        return Err("no webui sources".into());
    }
    for f in ["package.json", "package-lock.json", ".parcelrc"] {
        if webui.join(f).exists() {
            std::fs::create_dir_all(work).map_err(|e| e.to_string())?;
            std::fs::copy(webui.join(f), work.join(f)).map_err(|e| e.to_string())?;
        }
    }
    copy_tree(&webui.join("src"), &work.join("src")).map_err(|e| e.to_string())?;
    let npm = |args: &[&str]| -> Result<(), String> {
        let out = Command::new("npm").args(args).current_dir(work).output().map_err(|e| format!("npm: {e}"))?;
        if out.status.success() { Ok(()) } else { Err(format!("npm {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or(""))) }
    };
    if !work.join("node_modules").exists() {
        npm(&["ci", "--no-audit", "--no-fund"])?;
    }
    npm(&["run", "build"])?;
    let dist = work.join("dist");
    if dist.join("index.html").exists() { Ok(dist) } else { Err("the build produced no index.html".into()) }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let webui = manifest.join("../../webui");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-changed={}", webui.join("dist").display());
    println!("cargo:rerun-if-changed={}", webui.join("src").display());
    println!("cargo:rerun-if-changed={}", webui.join("package.json").display());
    println!("cargo:rerun-if-env-changed=SUBNET_WEBUI_BUILD");
    let local = webui.join("dist");
    let dist = if local.join("index.html").exists() {
        local
    } else {
        match npm_build(&webui, &out.join("webui")) {
            Ok(d) => d,
            Err(why) => {
                println!("cargo:warning=the web UI isn't built ({why}); embedding a placeholder page (run npm run build in webui/)");
                let p = out.join("webui-placeholder");
                std::fs::create_dir_all(&p).expect("creating the placeholder dir");
                std::fs::write(
                    p.join("index.html"),
                    "<!doctype html><meta charset=utf-8><body style='background:#000;color:#ddd;font:14px monospace;padding:2em'>\
                     The web UI isn't built. Run <code>npm install &amp;&amp; npm run build</code> in <code>webui/</code> and rebuild the hub.",
                )
                .expect("writing the placeholder");
                p
            }
        }
    };
    println!("cargo:rustc-env=SUBNET_WEBUI_DIST={}", dist.canonicalize().unwrap_or(dist).display());
}
