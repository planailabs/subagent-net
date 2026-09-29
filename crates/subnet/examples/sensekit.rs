//! Portable sense programs for tests and demos:
//! - `sensekit emit <json>...`          print each JSON value as a line, then idle
//! - `sensekit stream <text> <ms>`      write <text> as raw bytes every <ms>
//! - `sensekit words`                   stage: raw bytes in, `{"text": …}` per chunk out
//! - `sensekit double`                  stage: `{"n": x}` lines in, `{"n": 2x}` out

use std::io::{BufRead, Read, Write};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out = std::io::stdout();
    match args.first().map(String::as_str) {
        Some("emit") => {
            for a in &args[1..] {
                writeln!(out, "{a}").unwrap();
                out.flush().unwrap();
            }
            // Stay alive like a sensor would; the node stops us.
            std::thread::sleep(Duration::from_secs(3600));
        }
        Some("stream") => {
            let ms: u64 = args[2].parse().unwrap();
            loop {
                out.write_all(args[1].as_bytes()).unwrap();
                out.flush().unwrap();
                std::thread::sleep(Duration::from_millis(ms));
            }
        }
        Some("words") => {
            let mut buf = [0u8; 4096];
            let mut stdin = std::io::stdin();
            loop {
                let n = stdin.read(&mut buf).unwrap();
                if n == 0 {
                    return;
                }
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                writeln!(out, "{}", serde_json::json!({ "text": text })).unwrap();
                out.flush().unwrap();
            }
        }
        Some("double") => {
            for line in std::io::stdin().lock().lines() {
                let v: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
                let n = v["n"].as_i64().unwrap_or(0);
                writeln!(out, "{}", serde_json::json!({ "n": n * 2 })).unwrap();
                out.flush().unwrap();
            }
        }
        _ => panic!("usage: sensekit emit|stream|words|double"),
    }
}
