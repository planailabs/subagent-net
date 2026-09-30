use super::*;

fn design_example() -> String {
    let s = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../DESIGN.md")).unwrap();
    let a = s.find("```hcl\n").expect("DESIGN.md has an hcl block") + 7;
    let b = a + s[a..].find("```").unwrap();
    s[a..b].to_string()
}

fn parse(s: &str) -> Result<Cluster, Error> {
    Cluster::parse(&[("test.hcl", s)])
}

const BASE: &str = r#"
node "n1" {}
agent "a" {
  credential {
    base_url = "http://x/v1"
  }
  model = "m"
  nodes = ["n1"]
}
"#;

fn with(extra: &str) -> Result<Cluster, Error> {
    parse(&format!("{BASE}\n{extra}"))
}

fn err(extra: &str) -> String {
    match with(extra) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected an error for:\n{extra}"),
    }
}

#[test]
fn design_md_example_is_valid() {
    let c = parse(&design_example()).unwrap();
    assert_eq!(c.users["maciej"].role, Role::Admin);
    assert_eq!(c.agents["deepseek-flash"].budget.max_tokens, Some(200000));
    assert_eq!(c.agents["deepseek-flash"].system_prompt, "You are fast and terse.\n");
    assert_eq!(c.mcps["web"].credential.as_ref().unwrap().prefix, "Bearer ");
    let stages: Vec<_> = c.senses["hall-speech"].stage.keys().cloned().collect();
    assert_eq!(stages, ["stt", "words"], "stage order is kept");
    assert_eq!(c.routes["door-open"].deliver.len(), 2);
    assert_eq!(c.routes["door-open"].throttle.unwrap().to_string(), "1/10s");
    assert_eq!(c.routes["log-everything"].batch.as_ref().unwrap().max, Some(500));
    assert_eq!(c.senses["hall-mic"].source.publishes(), Some("pcm_s16le/16000"));
}

#[test]
fn example_file_matches_design() {
    let f = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/cluster.hcl")).unwrap();
    assert_eq!(parse(&f).unwrap(), parse(&design_example()).unwrap());
}

#[test]
fn node_config_selects_what_a_node_runs() {
    let c = parse(&design_example()).unwrap();
    let gpu = c.node_config("gpu-1").unwrap();
    assert_eq!(gpu.agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["deepseek-flash"]);
    assert_eq!(gpu.mcps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["memory"]);
    assert_eq!(gpu.senses.keys().collect::<Vec<_>>(), ["hall-speech", "hourly"]);
    assert_eq!(gpu.relay_in, ["hall-mic"], "hall-mic is published on pi-hall");
    let pi = c.node_config("pi-hall").unwrap();
    assert_eq!(pi.relay_out, ["hall-mic"]);
    assert!(pi.relay_in.is_empty() && gpu.relay_out.is_empty());
    let laptop = c.node_config("laptop").unwrap();
    assert_eq!(laptop.capacity, 4);
    assert_eq!(laptop.agents[0].id, gpu.agents[0].id, "same type, same identity on every node");
    assert!(c.node_config("nope").is_none());
}

#[test]
fn identity_ignores_placement_but_not_behaviour() {
    let c = with("").unwrap();
    let id = c.agent_id("a").unwrap();
    assert!(id.starts_with("a@"));
    let moved = parse(&format!("{}\nnode \"n2\" {{}}", BASE.replace(r#"nodes = ["n1"]"#, r#"nodes = ["n1", "n2"]"#))).unwrap();
    assert_eq!(moved.agent_id("a").unwrap(), id);
    let changed = parse(&BASE.replace(r#"model = "m""#, r#"model = "m2""#)).unwrap();
    assert_ne!(changed.agent_id("a").unwrap(), id);
}

#[test]
fn several_files_merge_but_names_are_unique() {
    let a = ("a.hcl", BASE);
    let b = ("b.hcl", "mixture \"x\" {\n  agent = \"a\"\n}\n");
    let c = Cluster::parse(&[a, b]).unwrap();
    assert!(c.mixtures.contains_key("x"));
    let dup = Cluster::parse(&[a, ("b.hcl", "node \"n1\" {}")]).unwrap_err().to_string();
    assert!(dup.contains("already declared in a.hcl"), "{dup}");
    let dup = parse(&format!("{BASE}\nnode \"n1\" {{}}")).unwrap_err().to_string();
    assert!(dup.contains("already declared"), "{dup}");
}

#[test]
fn unknown_blocks_fields_and_syntax_are_rejected() {
    assert!(err("robot \"r\" {}").contains("unknown block"));
    assert!(err("mixture \"x\" {\n  agent = \"a\"\n  colour = \"red\"\n}").contains("colour"));
    assert!(err("mixture \"x\" {").contains("test.hcl"));
    assert!(err("mixture {\n  agent = \"a\"\n}").contains("name label"));
    assert!(err("top = 1").contains("top-level attribute"));
}

#[test]
fn references_are_checked() {
    assert!(err("mixture \"x\" {\n  agent = \"ghost\"\n}").contains("unknown agent"));
    assert!(err("mixture \"x\" {\n  agent = \"a\"\n  mcp = [\"ghost\"]\n}").contains("unknown mcp"));
    assert!(err("resident \"r\" {\n  mixture = \"ghost\"\n}").contains("unknown mixture"));
    let e = err("sense \"s\" {\n  node = \"ghost\"\n  source { exec = [\"x\"] }\n}");
    assert!(e.contains("unknown node"), "{e}");
    let e = err("route \"r\" {\n  from = \"ghost\"\n  deliver { mailbox = \"m\" }\n}");
    assert!(e.contains("unknown sense"), "{e}");
    let s = "sense \"s\" {\n  node = \"n1\"\n  source { exec = [\"x\"] }\n}\n";
    assert!(err(&format!("{s}route \"r\" {{\n  from = \"s\"\n  deliver {{ send = \"nobody\" }}\n}}")).contains("resident"));
    assert!(err(&format!("{s}route \"r\" {{\n  from = \"s\"\n  deliver {{ spawn = \"ghost\" }}\n}}")).contains("unknown mixture"));
}

#[test]
fn exclusive_options_are_checked() {
    let mcp = |body: &str| err(&format!("mcp \"m\" {{\n  nodes = [\"n1\"]\n{body}\n}}"));
    assert!(mcp("").contains("exactly one of command or url"));
    assert!(mcp("  command = [\"x\"]\n  url = \"http://x\"").contains("exactly one"));
    let sense = |src: &str| err(&format!("sense \"s\" {{\n  node = \"n1\"\n  source {{\n{src}\n  }}\n}}"));
    assert!(sense("").contains("exactly one of exec"));
    assert!(sense("    exec = [\"x\"]\n    webhook = { path = \"/x\" }").contains("exactly one"));
    assert!(sense("    timer = { every = \"1s\", cron = \"* * * * *\" }").contains("exactly one of every or cron"));
    assert!(sense("    timer = { cron = \"not a cron\" }").contains("cron"));
    assert!(sense("    webhook = { path = \"x\" }").contains("start with /"));
    assert!(sense("    stream = \"nothing\"").contains("unknown stream"));
    let s = "sense \"s\" {\n  node = \"n1\"\n  source { exec = [\"x\"] }\n}\n";
    let d = |dl: &str| err(&format!("{s}route \"r\" {{\n  from = \"s\"\n{dl}\n}}"));
    assert!(d("  deliver {\n    mailbox = \"m\"\n    send = \"x\"\n  }").contains("exactly one of spawn"));
    assert!(d("  deliver {\n    mailbox = \"m\"\n    prompt = \"'x'\"\n  }").contains("prompt goes with spawn"));
    assert!(d("  max_active = 2\n  deliver { mailbox = \"m\" }").contains("max_active"));
    assert!(d("  throttle = \"fast\"\n  deliver { mailbox = \"m\" }").contains("rate"));
}

#[test]
fn stream_publishers_have_no_stages_and_no_routes() {
    let publ = "sense \"mic\" {\n  node = \"n1\"\n  source {\n    exec = [\"rec\"]\n    stream = \"pcm\"\n  }\n}\n";
    with(publ).unwrap();
    assert!(err(&format!("{publ}route \"r\" {{\n  from = \"mic\"\n  deliver {{ mailbox = \"m\" }}\n}}")).contains("publishes a stream"));
    let staged = publ.replace("  }\n}\n", "  }\n  stage \"x\" { filter = \"true\" }\n}\n");
    assert!(err(&staged).contains("no stages"));
    let sub = "sense \"words\" {\n  node = \"n1\"\n  source { stream = \"mic\" }\n  stage \"stt\" { exec = [\"stt\"] }\n}\n";
    with(&format!("{publ}{sub}")).unwrap();
    let bad = sub.replace("exec = [\"stt\"]", "filter = \"true\"");
    assert!(err(&format!("{publ}{bad}")).contains("first stage must be exec"));
}

#[test]
fn bad_names_are_rejected() {
    assert!(err("mixture \"a.b\" {\n  agent = \"a\"\n}").contains("may not"));
    assert!(err("mixture \"a\" {\n  agent = \"a\"\n}").contains("same name"));
    assert!(err("user \"x\" { role = \"admin\" }\nclient \"x\" { role = \"viewer\" }").contains("both"));
    assert!(err("user \"x\" { role = \"god\" }").contains("god"));
}

#[test]
fn diff_reports_changes_per_block() {
    let old = with("mixture \"x\" {\n  agent = \"a\"\n}").unwrap();
    let new = parse(&format!("{}\nnode \"n2\" {{}}", BASE.replace(r#"model = "m""#, r#"model = "m2""#))).unwrap();
    let d = old.diff(&new);
    assert!(d.contains(&Change { kind: "agent".into(), name: "a".into(), action: Action::Changed }));
    assert!(d.contains(&Change { kind: "node".into(), name: "n2".into(), action: Action::Added }));
    assert!(d.contains(&Change { kind: "mixture".into(), name: "x".into(), action: Action::Removed }));
    assert_eq!(d.len(), 3);
    assert!(new.diff(&new).is_empty());
}

#[test]
fn every_example_file_is_valid() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
    let mut n = 0;
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "hcl") {
            let text = std::fs::read_to_string(&p).unwrap();
            Cluster::parse(&[(p.to_str().unwrap(), &text)]).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            n += 1;
        }
    }
    assert!(n >= 2);
}

#[test]
fn mcp_servers_are_lazy_unless_told_and_mixtures_may_route() {
    let c = with(
        r#"
mcp "web" {
  command = ["web-mcp"]
  nodes   = ["n1"]
}
mcp "world" {
  url   = "http://w/mcp"
  nodes = ["n1"]
  lazy  = false
}
mixture "m" {
  agent  = "a"
  mcp    = ["web", "world"]
  router {
    top_k = 5
  }
}
mixture "plain" {
  agent = "a"
}
"#,
    )
    .unwrap();
    assert!(c.mcps["web"].lazy);
    assert!(!c.mcps["world"].lazy);
    assert_eq!(c.mixtures["m"].router, Some(RouterDef { top_k: 5, min_score: 0.78 }));
    assert_eq!(c.mixtures["plain"].router, None);
    assert!(err("mixture \"m\" {\n agent = \"a\"\n router { top_k = 0 }\n}").contains("top_k"));
}

#[test]
fn compaction_is_on_unless_turned_off() {
    let with_compact = |c: &str| parse(&BASE.replace("nodes = [\"n1\"]", &format!("nodes = [\"n1\"]\n  {c}")));
    let on = parse(BASE).unwrap();
    let a = &on.agents["a"];
    assert_eq!(a.compact.spec(&a.executor), Some(subnet_core::agent::Compact { at_tokens: 96_000, keep: 8 }));
    let tuned = with_compact("compact {\n    at_tokens = 50000\n  }").unwrap();
    assert_eq!(tuned.agents["a"].compact.at_tokens, 50_000);
    assert_eq!(tuned.agents["a"].compact.keep, 8, "unset fields keep their defaults");
    assert_ne!(tuned.agent_id("a"), on.agent_id("a"));
    let off = with_compact("compact {\n    enabled = false\n  }").unwrap();
    let a = &off.agents["a"];
    assert_eq!(a.compact.spec(&a.executor), None);
    assert!(with_compact("compact {\n    keep = 0\n  }").unwrap_err().to_string().contains("compact"));
}
