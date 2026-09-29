// The hub embeds webui/dist. If the UI hasn't been built, embed a page that
// says so, so the Rust build never depends on node.
fn main() {
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../webui/dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    if !dist.join("index.html").exists() {
        std::fs::create_dir_all(&dist).expect("creating webui/dist");
        std::fs::write(
            dist.join("index.html"),
            "<!doctype html><meta charset=utf-8><body style='background:#000;color:#ddd;font:14px monospace;padding:2em'>\
             The web UI isn't built. Run <code>npm install &amp;&amp; npm run build</code> in <code>webui/</code> and rebuild the hub.",
        )
        .expect("writing placeholder index.html");
        println!("cargo:warning=webui/dist was missing; embedded a placeholder page (run npm run build in webui/)");
    }
}
