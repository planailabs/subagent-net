//! CLI front-end: a clap subcommand per operation, built from its argument
//! schema. Path parameters are positional, other fields are `--flags`, and
//! `--json` passes a whole argument object.

use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{Map, Value};

use crate::OpMeta;
use crate::http::{resolve, types};

pub fn command_name(op: &str) -> String {
    op.replace('_', "-")
}

fn flag_name(field: &str) -> String {
    field.replace('_', "-")
}

fn is_required(meta: &OpMeta, field: &str) -> bool {
    meta.args["required"].as_array().is_some_and(|r| r.iter().any(|x| x == field))
}

fn enum_values(root: &Value, s: &Value) -> Vec<String> {
    let s = resolve(root, s);
    let mut out: Vec<String> = s["enum"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
    for k in ["oneOf", "anyOf"] {
        for b in s[k].as_array().into_iter().flatten() {
            out.extend(enum_values(root, b));
            if let Some(c) = resolve(root, b).get("const").and_then(Value::as_str) {
                out.push(c.to_string());
            }
        }
    }
    out
}

/// One subcommand per operation.
pub fn commands<'a>(metas: impl IntoIterator<Item = &'a OpMeta>) -> Vec<Command> {
    metas.into_iter().map(command).collect()
}

pub fn command(meta: &OpMeta) -> Command {
    let mut cmd = Command::new(command_name(meta.name)).about(meta.summary);
    let path = meta.path_params();
    let empty = Map::new();
    let props = meta.args["properties"].as_object().unwrap_or(&empty);
    // Positional path parameters first, in path order.
    for p in &path {
        let s = &props.get(*p).cloned().unwrap_or_default();
        let mut a = Arg::new(p.to_string()).required_unless_present("__json").value_name(p.to_uppercase());
        if let Some(d) = resolve(&meta.args, s)["description"].as_str().or(s["description"].as_str()) {
            a = a.help(d.to_string());
        }
        cmd = cmd.arg(a);
    }
    for (k, s) in props.iter().filter(|(k, _)| !path.contains(&k.as_str())) {
        let ts = types(&meta.args, s);
        let required = is_required(meta, k);
        let mut a = Arg::new(k.clone()).long(flag_name(k));
        if let Some(d) = s["description"].as_str().or(resolve(&meta.args, s)["description"].as_str()) {
            a = a.help(d.to_string());
        }
        a = if ts.iter().any(|t| t == "boolean") && !required {
            a.action(ArgAction::SetTrue)
        } else if ts.iter().any(|t| t == "array") {
            a.action(ArgAction::Append).value_name("VALUE")
        } else {
            let vals = enum_values(&meta.args, s);
            let mut a = a.value_name(k.to_uppercase());
            if required {
                a = a.required_unless_present("__json");
            }
            if vals.is_empty() { a } else { a.value_parser(clap::builder::PossibleValuesParser::new(vals)) }
        };
        cmd = cmd.arg(a);
    }
    cmd.arg(
        Arg::new("__json")
            .long("json")
            .value_name("JSON")
            .help("All arguments as one JSON object; flags override its fields"),
    )
}

/// A CLI string as the JSON value the schema wants.
fn value(root: &Value, s: &Value, raw: &str) -> Value {
    let ts = types(root, s);
    if ts.iter().any(|t| t == "string") && !ts.iter().any(|t| t == "object" || t == "array") {
        return Value::String(raw.into());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.into()))
}

/// Builds the argument object from parsed matches.
pub fn args_from(meta: &OpMeta, m: &ArgMatches) -> Result<Value, String> {
    let mut out = match m.get_one::<String>("__json") {
        Some(j) => match serde_json::from_str(j).map_err(|e| format!("--json: {e}"))? {
            Value::Object(o) => o,
            _ => return Err("--json must be an object".into()),
        },
        None => Map::new(),
    };
    let empty = Map::new();
    let props = meta.args["properties"].as_object().unwrap_or(&empty);
    for (k, s) in props {
        let ts = types(&meta.args, s);
        if ts.iter().any(|t| t == "boolean") && !is_required(meta, k) && !meta.path_params().contains(&k.as_str()) {
            if m.get_flag(k) {
                out.insert(k.clone(), Value::Bool(true));
            }
            continue;
        }
        if ts.iter().any(|t| t == "array") {
            if let Some(vs) = m.get_many::<String>(k) {
                let item = resolve(&meta.args, s).get("items").cloned().unwrap_or_default();
                out.insert(k.clone(), Value::Array(vs.map(|v| value(&meta.args, &item, v)).collect()));
            }
            continue;
        }
        if let Some(v) = m.get_one::<String>(k) {
            out.insert(k.clone(), value(&meta.args, s, v));
        }
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use serde_json::json;

    fn parse(meta: &OpMeta, argv: &[&str]) -> Result<Value, String> {
        let root = Command::new("t").subcommands(commands([meta]));
        let m = root.try_get_matches_from(argv).map_err(|e| e.to_string())?;
        let (_, sub) = m.subcommand().unwrap();
        args_from(meta, sub)
    }

    #[test]
    fn builds_args_from_flags_and_positionals() {
        let r = registry();
        let kick = r.meta("kick_thing").unwrap();
        let v = parse(kick, &["t", "kick-thing", "t9", "--mode", "hard", "--tree", "--tags", "a", "--tags", "b", "--times", "2"]).unwrap();
        assert_eq!(v, json!({"id":"t9","mode":"hard","tree":true,"tags":["a","b"],"times":2}));
        let v = parse(kick, &["t", "kick-thing", "t9", "--mode", "soft"]).unwrap();
        assert_eq!(v, json!({"id":"t9","mode":"soft"}));
    }

    #[test]
    fn validates_enums_and_required() {
        let r = registry();
        let kick = r.meta("kick_thing").unwrap();
        assert!(parse(kick, &["t", "kick-thing", "t9", "--mode", "medium"]).unwrap_err().contains("medium"));
        assert!(parse(kick, &["t", "kick-thing", "--mode", "soft"]).is_err(), "id is required");
        let add = r.meta("add").unwrap();
        assert!(parse(add, &["t", "add", "--a", "1"]).is_err(), "b is required");
        assert_eq!(parse(add, &["t", "add", "--a", "1", "--b", "2"]).unwrap(), json!({"a":1,"b":2}));
    }

    #[test]
    fn json_flag_merges_under_flags() {
        let r = registry();
        let add = r.meta("add").unwrap();
        let v = parse(add, &["t", "add", "--json", r#"{"a":5,"b":6}"#, "--a", "1"]).unwrap();
        assert_eq!(v, json!({"a":1,"b":6}));
    }

    #[test]
    fn help_uses_descriptions() {
        let r = registry();
        let mut c = command(r.meta("kick_thing").unwrap());
        let help = c.render_long_help().to_string();
        assert!(help.contains("Who to kick."), "{help}");
        assert!(help.contains("Kick a thing."));
    }
}
