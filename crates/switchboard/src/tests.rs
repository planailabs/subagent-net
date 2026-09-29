use super::*;
use subnet_cluster::Cluster;

fn route(body: &str) -> Route {
    let text = format!(
        "node \"n\" {{}}\nagent \"a\" {{\n  credential {{\n    base_url = \"x\"\n  }}\n  model = \"m\"\n  nodes = [\"n\"]\n}}\nresident \"r\" {{\n  mixture = \"a\"\n}}\nmcp \"mem\" {{\n  url = \"http://x\"\n  nodes = [\"n\"]\n}}\nsense \"s\" {{\n  node = \"n\"\n  source {{ exec = [\"x\"] }}\n}}\nroute \"rt\" {{\n  from = \"s\"\n{body}\n}}\n"
    );
    let c = Cluster::parse(&[("t.hcl", &text)]).unwrap();
    compile(&c).unwrap().pop().unwrap()
}

fn ev(n: u64, data: Value) -> SenseEvent {
    SenseEvent { id: format!("e{n}"), sense: "s".into(), at: 1000 + n, data }
}

#[test]
fn when_and_map() {
    let r = route("  when = \"event.state == 'open'\"\n  map = \"{'who': event.card, 'at': at}\"\n  deliver { mailbox = \"m\" }");
    let mut st = RouteState::default();
    assert!(st.offer(&r, &ev(1, json!({"state":"closed"})), 0).is_empty());
    let out = st.offer(&r, &ev(2, json!({"state":"open","card":"x7"})), 0);
    assert_eq!(out[0].event, json!({"who":"x7","at":1002}));
    assert_eq!((st.counters.seen, st.counters.filtered, st.counters.delivered), (2, 1, 1));
}

#[test]
fn expression_errors_are_counted_not_fatal() {
    let r = route("  when = \"event.missing.deeper == 1\"\n  deliver { mailbox = \"m\" }");
    let mut st = RouteState::default();
    assert!(st.offer(&r, &ev(1, json!({})), 0).is_empty());
    assert_eq!(st.counters.errors, 1);
    assert!(st.last_error.is_some());
    let r = route("  when = \"'not a bool'\"\n  deliver { mailbox = \"m\" }");
    assert!(RouteState::default().offer(&r, &ev(1, json!({})), 0).is_empty());
}

#[test]
fn bad_cel_is_a_compile_error() {
    let text = "node \"n\" {}\nsense \"s\" {\n  node = \"n\"\n  source { exec = [\"x\"] }\n}\nroute \"r\" {\n  from = \"s\"\n  when = \"event.x ==\"\n  deliver { mailbox = \"m\" }\n}\n";
    let c = Cluster::parse(&[("t.hcl", text)]).unwrap();
    assert!(compile(&c).unwrap_err().contains("route \"r\""));
}

#[test]
fn throttle_limits_deliveries_per_window() {
    let r = route("  throttle = \"2/10s\"\n  deliver { mailbox = \"m\" }");
    let mut st = RouteState::default();
    let n = |st: &mut RouteState, t| st.offer(&r, &ev(t, json!({})), t).len();
    assert_eq!(n(&mut st, 0) + n(&mut st, 1) + n(&mut st, 2), 2);
    assert_eq!(st.counters.throttled, 1);
    assert_eq!(n(&mut st, 10_000), 1, "window slid past the first delivery");
}

#[test]
fn debounce_delivers_the_last_event_after_quiet() {
    let r = route("  debounce = \"2s\"\n  deliver { mailbox = \"m\" }");
    let mut st = RouteState::default();
    for (t, v) in [(0, 1), (500, 2), (1500, 3)] {
        assert!(st.offer(&r, &ev(t, json!({"v": v})), t).is_empty());
    }
    assert_eq!(st.next_due(), Some(3500));
    assert!(st.tick(&r, 3000).is_empty());
    let out = st.tick(&r, 3500);
    assert_eq!(out[0].event, json!({"v":3}));
    assert_eq!(st.counters.debounced, 2);
    assert_eq!(st.next_due(), None);
}

#[test]
fn batch_by_size_and_by_window() {
    let r = route("  batch = { window = \"1m\", max = 3 }\n  deliver { mailbox = \"m\" }");
    let mut st = RouteState::default();
    assert!(st.offer(&r, &ev(0, json!(1)), 0).is_empty());
    assert!(st.offer(&r, &ev(1, json!(2)), 1).is_empty());
    let out = st.offer(&r, &ev(2, json!(3)), 2);
    assert_eq!(out[0].batch, vec![json!(1), json!(2), json!(3)]);
    assert_eq!(out[0].ids, ["e0", "e1", "e2"]);
    st.offer(&r, &ev(3, json!(4)), 100);
    assert_eq!(st.next_due(), Some(60_100));
    assert_eq!(st.tick(&r, 60_100)[0].batch, vec![json!(4)]);
}

#[test]
fn dedupe_drops_repeated_keys_within_window() {
    let r = route("  dedupe = { key = \"event.card\", within = \"1m\" }\n  deliver { mailbox = \"m\" }");
    let mut st = RouteState::default();
    let n = |st: &mut RouteState, t, c: &str| st.offer(&r, &ev(t, json!({"card": c})), t).len();
    assert_eq!(n(&mut st, 0, "a") + n(&mut st, 1, "a") + n(&mut st, 2, "b"), 2);
    assert_eq!(st.counters.deduped, 1);
    assert_eq!(n(&mut st, 60_000, "a"), 1, "window expired");
}

#[test]
fn actions_render_prompts_args_and_payloads() {
    let r = route(
        "  deliver {\n    spawn = \"a\"\n    prompt = \"'saw ' + event.what\"\n  }\n  deliver { send = \"r\" }\n  deliver { mailbox = \"box\" }\n  deliver {\n    mcp = { server = \"mem\", tool = \"store\", args = \"{'n': size(batch)}\" }\n  }",
    );
    let mut st = RouteState::default();
    let p = st.offer(&r, &ev(1, json!({"what":"a cat"})), 0).pop().unwrap();
    let a = r.actions(&p).unwrap();
    assert_eq!(a[0], Action::Spawn { mixture: "a".into(), prompt: "saw a cat".into() });
    assert_eq!(a[1], Action::Send { resident: "r".into(), content: r#"{"what":"a cat"}"#.into() });
    assert_eq!(a[2], Action::Mailbox { name: "box".into(), content: r#"{"what":"a cat"}"#.into() });
    assert_eq!(a[3], Action::Mcp { server: "mem".into(), tool: "store".into(), args: json!({"n":1}) });
}

#[test]
fn default_prompt_names_the_route() {
    let r = route("  deliver { spawn = \"a\" }");
    let p = RouteState::default().offer(&r, &ev(1, json!({"x":1})), 0).pop().unwrap();
    let Action::Spawn { prompt, .. } = &r.actions(&p).unwrap()[0] else { panic!() };
    assert!(prompt.contains("route rt") && prompt.contains(r#"{"x":1}"#), "{prompt}");
}

#[test]
fn stages_filter_and_map_with_prev() {
    let mut f = Stage::filter("prev == null || event.state != prev.state").unwrap();
    let a = f.apply(json!({"state":"open"})).unwrap();
    assert!(a.is_some());
    assert!(f.apply(json!({"state":"open"})).unwrap().is_none(), "same state as before: bounce");
    assert!(f.apply(json!({"state":"closed"})).unwrap().is_some());
    let mut m = Stage::map("{'n': event.n * 2}").unwrap();
    assert_eq!(m.apply(json!({"n": 21})).unwrap(), Some(json!({"n": 42})));
}
