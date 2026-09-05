//! `js_to_json`/`json_to_js` move values between a guest `rquickjs::Value`
//! and a `serde_json::Value`; conversion caps depth, nodes, and copied bytes
//! so a self-referential or pathologically large value cannot exhaust memory.

#[allow(clippy::wildcard_imports)]
use super::*;

struct Conversion<'js> {
    nodes: usize,
    bytes: usize,
    max_bytes: usize,
    overflow: bool,
    truncate: bool,
    truncated: bool,
    string_limiter: Option<Function<'js>>,
}

impl<'js> Conversion<'js> {
    fn new(value: &Value<'js>, max_bytes: usize, truncate: bool) -> Self {
        let string_limiter = truncate.then(|| {
            value.ctx().eval::<Function, _>(
                "(value, max) => { const cut = value.length > max; return [cut ? value.slice(0, max) : value, cut]; }",
            )
        });
        let overflow = string_limiter
            .as_ref()
            .is_some_and(std::result::Result::is_err);
        Self {
            nodes: 0,
            bytes: 0,
            max_bytes,
            overflow,
            truncate,
            truncated: false,
            string_limiter: string_limiter.and_then(std::result::Result::ok),
        }
    }

    fn remaining(&self) -> usize {
        self.max_bytes.saturating_sub(self.bytes)
    }

    fn copy_string(
        &mut self,
        string: rquickjs::String<'js>,
        allow_prefix: bool,
    ) -> Option<std::string::String> {
        let mut guest_truncated = false;
        let string = if let Some(limiter) = &self.string_limiter {
            let limit = u32::try_from(self.remaining()).unwrap_or(u32::MAX);
            let bounded: rquickjs::Result<Array> = limiter.call((string, limit));
            let Ok(bounded) = bounded else {
                self.overflow = true;
                return None;
            };
            guest_truncated = bounded.get(1).unwrap_or(true);
            if guest_truncated && !allow_prefix {
                self.truncated = true;
                return None;
            }
            let Ok(string) = bounded.get(0) else {
                self.overflow = true;
                return None;
            };
            string
        } else {
            string
        };
        let Ok(text) = string.to_cstring() else {
            self.overflow = true;
            return None;
        };
        let remaining = self.remaining();
        if text.len() <= remaining {
            self.bytes += text.len();
            self.truncated |= guest_truncated;
            return Some(text.as_str().to_string());
        }
        if self.truncate {
            self.truncated = true;
            if allow_prefix {
                let end = text.as_str().floor_char_boundary(remaining);
                self.bytes += end;
                return Some(text.as_str()[..end].to_string());
            }
        } else {
            self.overflow = true;
        }
        None
    }
}

/// Convert a guest value to JSON, surfacing depth, node, or byte overflow as
/// the sandbox-error sentinel so the caller raises [`Error::Sandbox`].
pub(super) fn js_to_json(v: &Value<'_>) -> Json {
    js_to_json_with_mode(v, JS_TO_JSON_MAX_BYTES, false).0
}

/// Convert a guest result while copying no more than `max_bytes` of strings
/// and object keys into host allocations. The boolean reports byte-budget
/// truncation. Structural depth and node overflows remain sandbox errors.
pub(super) fn js_to_json_with_limit(v: &Value<'_>, max_bytes: usize) -> (Json, bool) {
    js_to_json_with_mode(v, max_bytes, true)
}

fn js_to_json_with_mode(v: &Value<'_>, max_bytes: usize, truncate: bool) -> (Json, bool) {
    let mut conversion = Conversion::new(v, max_bytes, truncate);
    let json = js_to_json_bounded(v, 0, &mut conversion);
    if conversion.overflow {
        (
            json!({ SANDBOX_ERROR_KEY: "value too deep or too large to convert" }),
            false,
        )
    } else {
        (json, conversion.truncated)
    }
}

pub(super) fn json_to_js<'js>(ctx: &Ctx<'js>, v: &Json) -> rquickjs::Result<Value<'js>> {
    let val: Value = match v {
        Json::Null => Value::new_null(ctx.clone()),
        Json::Bool(b) => b.into_js(ctx)?,
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                #[allow(clippy::cast_precision_loss)]
                (i as f64).into_js(ctx)?
            } else {
                n.as_f64().unwrap_or(f64::NAN).into_js(ctx)?
            }
        }
        Json::String(s) => s.into_js(ctx)?,
        Json::Array(arr) => {
            let a = Array::new(ctx.clone())?;
            for (i, item) in arr.iter().enumerate() {
                a.set(i, json_to_js(ctx, item)?)?;
            }
            a.into_value()
        }
        Json::Object(map) => {
            let o = Object::new(ctx.clone())?;
            for (k, v) in map {
                o.set(k.as_str(), json_to_js(ctx, v)?)?;
            }
            o.into_value()
        }
    };
    Ok(val)
}

/// `undefined` maps to `Null` so a guest `return;` yields `Value::Null`.
/// Functions, symbols, and opaque objects use visible sentinel values.
fn js_to_json_bounded<'js>(v: &Value<'js>, depth: usize, conversion: &mut Conversion<'js>) -> Json {
    if conversion.overflow || conversion.truncated {
        return Json::Null;
    }
    if depth > JS_TO_JSON_MAX_DEPTH || conversion.nodes > JS_TO_JSON_MAX_NODES {
        conversion.overflow = true;
        return Json::Null;
    }
    conversion.nodes += 1;
    if v.is_undefined() || v.is_null() {
        return Json::Null;
    }
    if let Some(b) = v.as_bool() {
        return json!(b);
    }
    if let Some(i) = v.as_int() {
        return json!(i);
    }
    if let Some(f) = v.as_float() {
        if f.fract() == 0.0 && f.abs() < 9.0072e15 {
            #[allow(clippy::cast_possible_truncation)]
            return json!(f as i64);
        }
        return json!(f);
    }
    if let Some(string) = v.as_string() {
        return conversion
            .copy_string(string.clone(), true)
            .map_or(Json::Null, Json::String);
    }
    if let Some(sym) = v.as_symbol() {
        let desc = sym
            .description()
            .ok()
            .and_then(Value::into_string)
            .and_then(|string| conversion.copy_string(string, true))
            .unwrap_or_default();
        return json!(format!("Symbol({desc})"));
    }
    if let Some(arr) = v.as_array() {
        if conversion.nodes + arr.len() > JS_TO_JSON_MAX_NODES {
            conversion.overflow = true;
            return Json::Null;
        }
        let mut out = Vec::with_capacity(arr.len().min(64));
        for item in arr.iter::<Value>() {
            if conversion.overflow || conversion.truncated {
                break;
            }
            let item = item.unwrap_or_else(|_| Value::new_undefined(v.ctx().clone()));
            out.push(js_to_json_bounded(&item, depth + 1, conversion));
        }
        return Json::Array(out);
    }
    if let Some(function) = v.as_function() {
        let name = function
            .as_object()
            .and_then(|object| object.get::<_, rquickjs::String>("name").ok())
            .and_then(|string| conversion.copy_string(string, true))
            .unwrap_or_default();
        return sentinel_for("function", Some(&name), function.is_constructor());
    }
    if let Some(object) = v.as_object() {
        return object_to_json(object, depth, conversion);
    }
    if let Some(big) = v.as_big_int() {
        return big.clone().to_i64().map_or_else(
            |_| json!("BigInt(<unconverted>)"),
            |i| json!(format!("{i}n")),
        );
    }
    Json::Null
}

fn object_to_json<'js>(
    object: &rquickjs::Object<'js>,
    depth: usize,
    conversion: &mut Conversion<'js>,
) -> Json {
    let mut map = serde_json::Map::new();
    let string_only = rquickjs::object::Filter::new().string();
    for (key, value) in object
        .own_props::<rquickjs::String, Value>(string_only)
        .flatten()
    {
        if conversion.overflow || conversion.truncated {
            break;
        }
        let Some(key) = conversion.copy_string(key, false) else {
            break;
        };
        map.insert(key, js_to_json_bounded(&value, depth + 1, conversion));
    }

    if map.is_empty() && !conversion.truncated {
        let prototype_name = object.get_prototype().and_then(|prototype| {
            prototype
                .get::<_, Function>("constructor")
                .ok()
                .and_then(|constructor| {
                    constructor
                        .as_object()
                        .and_then(|object| object.get::<_, rquickjs::String>("name").ok())
                })
                .and_then(|string| conversion.copy_string(string, true))
        });
        if let Some(name) = prototype_name.filter(|name| !name.is_empty() && name != "Object") {
            return sentinel_for("object", Some(name.as_str()), false);
        }
    }
    Json::Object(map)
}

fn sentinel_for(kind: &str, name: Option<&str>, is_constructor: bool) -> Json {
    let mut map = serde_json::Map::new();
    map.insert("__lofi_opaque_kind__".to_string(), json!(kind));
    if let Some(name) = name {
        map.insert("__lofi_opaque_name__".to_string(), json!(name));
    }
    if is_constructor {
        map.insert("__lofi_opaque_constructor__".to_string(), json!(true));
    }
    Json::Object(map)
}
