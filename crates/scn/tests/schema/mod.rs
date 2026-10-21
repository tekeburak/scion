//! Checks `scn --json` output against `schemas/v1`. Only the JSON Schema
//! keywords those files use are known here; any other keyword fails the
//! check, so a schema cannot promise more than is checked.

use std::path::Path;

use serde_json::Value;

fn load(file: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas/v1")
        .join(file);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Parse `stdout` as one JSON document and panic unless it matches
/// `schemas/v1/<name>.json`; returns the document.
pub fn check(name: &str, stdout: &str) -> Value {
    let v: Value = serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("{name}: not one JSON document ({e}): {stdout}"));
    let file = format!("{name}.json");
    let doc = load(&file);
    let errors = errors(&file, &doc, &doc, &v, "$");
    assert!(errors.is_empty(), "{name}: {errors:#?}\n{v:#}");
    v
}

fn type_matches(name: &str, v: &Value) -> bool {
    match name {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        other => panic!("unknown type {other}"),
    }
}

/// Where `v` (at `at`) breaks `schema`, which sits in `root`, read from `file`.
fn errors(file: &str, root: &Value, schema: &Value, v: &Value, at: &str) -> Vec<String> {
    let mut out = Vec::new();
    let schema = schema.as_object().expect("a schema is an object");
    for (key, arg) in schema {
        match key.as_str() {
            "$schema" | "$id" | "title" | "description" | "$defs" => {}
            "$ref" => {
                let r = arg.as_str().unwrap();
                let (target, pointer) = r.split_once('#').unwrap_or((r, ""));
                let (file, doc) = if target.is_empty() {
                    (file.to_owned(), root.clone())
                } else {
                    (target.to_owned(), load(target))
                };
                let sub = doc.pointer(pointer).unwrap_or_else(|| panic!("no {r}"));
                out.extend(errors(&file, &doc, sub, v, at));
            }
            "anyOf" => {
                let arms = arg.as_array().unwrap();
                if !arms.iter().any(|s| errors(file, root, s, v, at).is_empty()) {
                    out.push(format!("{at}: matches no anyOf arm"));
                }
            }
            "type" => {
                let ok = match arg {
                    Value::String(t) => type_matches(t, v),
                    Value::Array(ts) => ts.iter().any(|t| type_matches(t.as_str().unwrap(), v)),
                    _ => panic!("bad type {arg}"),
                };
                if !ok {
                    out.push(format!("{at}: {v} is not {arg}"));
                }
            }
            "enum" => {
                if !arg.as_array().unwrap().contains(v) {
                    out.push(format!("{at}: {v} is not one of {arg}"));
                }
            }
            "required" => {
                if let Some(o) = v.as_object() {
                    for k in arg.as_array().unwrap() {
                        if !o.contains_key(k.as_str().unwrap()) {
                            out.push(format!("{at}: missing {k}"));
                        }
                    }
                }
            }
            "properties" => {
                if let Some(o) = v.as_object() {
                    for (k, s) in arg.as_object().unwrap() {
                        if let Some(x) = o.get(k) {
                            out.extend(errors(file, root, s, x, &format!("{at}.{k}")));
                        }
                    }
                }
            }
            "additionalProperties" => {
                let known = schema.get("properties").and_then(Value::as_object);
                if let Some(o) = v.as_object() {
                    for (k, x) in o {
                        if known.is_some_and(|p| p.contains_key(k)) {
                            continue;
                        }
                        match arg {
                            Value::Bool(false) => out.push(format!("{at}: unexpected {k:?}")),
                            Value::Bool(true) => {}
                            s => out.extend(errors(file, root, s, x, &format!("{at}.{k}"))),
                        }
                    }
                }
            }
            "items" => {
                if let Some(a) = v.as_array() {
                    for (i, x) in a.iter().enumerate() {
                        out.extend(errors(file, root, arg, x, &format!("{at}[{i}]")));
                    }
                }
            }
            "minLength" | "maxLength" => {
                if let Some(s) = v.as_str() {
                    let (n, bound) = (s.chars().count() as u64, arg.as_u64().unwrap());
                    if (key == "minLength" && n < bound) || (key == "maxLength" && n > bound) {
                        out.push(format!("{at}: length {n} breaks {key} {bound}"));
                    }
                }
            }
            "minimum" => {
                if let Some(n) = v.as_i64() {
                    if n < arg.as_i64().unwrap() {
                        out.push(format!("{at}: {n} is below {arg}"));
                    }
                }
            }
            other => panic!("{file}: keyword {other} is not checked here"),
        }
    }
    out
}
