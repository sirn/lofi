//! `js_to_json`/`json_to_js` move values between a guest `rquickjs::Value`
//! and a `serde_json::Value`; `js_to_json_bounded` caps depth/nodes/bytes so
//! a self-referential or pathologically nested structure surfaces as the
//! sandbox-error sentinel instead of exhausting memory.

#[allow(clippy::wildcard_imports)]
use super::*;
/// Convert a guest value to JSON, surfacing depth/node overflow as the
/// sandbox-error sentinel so the caller raises [`Error::Sandbox`].
pub(super) fn js_to_json(v: &Value<'_>) -> Json {
    let mut nodes = 0usize;
    let mut bytes = 0usize;
    let mut overflow = false;
    let json = js_to_json_bounded(v, 0, &mut nodes, &mut bytes, &mut overflow);
    if overflow {
        json!({ SANDBOX_ERROR_KEY: "value too deep or too large to convert" })
    } else {
        json
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

/// `undefined` maps to `Null` so a guest `return;` (or no return) yields
/// `Value::Null` rather than vanishing. Functions and symbols stringify.
/// Convert a `QuickJS` value to `JSON`, bounded by [`JS_TO_JSON_MAX_DEPTH`] and
/// [`JS_TO_JSON_MAX_NODES`] so a self-referential or pathologically nested
/// object can't recurse unboundedly and abort the harness. On overflow the
/// sentinel object `{ __lofi_sandbox_error__: ... }` is returned, which the
/// caller surfaces as [`Error::Sandbox`].
pub(super) fn js_to_json_bounded(
    v: &Value<'_>,
    depth: usize,
    nodes: &mut usize,
    bytes: &mut usize,
    overflow: &mut bool,
) -> Json {
    if *overflow {
        return Json::Null;
    }
    if depth > JS_TO_JSON_MAX_DEPTH || *nodes > JS_TO_JSON_MAX_NODES {
        *overflow = true;
        return Json::Null;
    }
    *nodes += 1;
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
    if v.is_string() {
        if let Some(s) = v.as_string() {
            // rquickjs 0.9 exposes no safe way to read a JS string length
            // without materializing an owned Rust `String`, so the budget is
            // checked after conversion. The check prevents the value from
            // being retained in the result (returns `null` on overflow); the
            // transient copy is bounded by the same QuickJS-heap limitation
            // documented as an accepted tradeoff (no safe memory-limit API,
            // `unsafe` forbidden).
            let text = s.to_string().unwrap_or_default();
            *bytes = bytes.saturating_add(text.len());
            if *bytes > JS_TO_JSON_MAX_BYTES {
                *overflow = true;
                return Json::Null;
            }
            return json!(text);
        }
    }
    if v.is_array() {
        if let Some(arr) = v.as_array() {
            if *nodes + arr.len() > JS_TO_JSON_MAX_NODES {
                *overflow = true;
                return Json::Null;
            }
            let mut out = Vec::with_capacity(arr.len().min(64));
            for item in arr.iter::<Value>() {
                if *overflow {
                    break;
                }
                let item = item.unwrap_or_else(|_| Value::new_undefined(v.ctx().clone()));
                out.push(js_to_json_bounded(&item, depth + 1, nodes, bytes, overflow));
            }
            return Json::Array(out);
        }
    }
    if v.is_object() {
        if let Some(obj) = v.as_object() {
            let mut map = serde_json::Map::new();
            for (k, val) in obj.props::<std::string::String, Value>().flatten() {
                if *overflow {
                    break;
                }
                *bytes = bytes.saturating_add(k.len());
                if *bytes > JS_TO_JSON_MAX_BYTES {
                    *overflow = true;
                    break;
                }
                map.insert(
                    k,
                    js_to_json_bounded(&val, depth + 1, nodes, bytes, overflow),
                );
            }
            return Json::Object(map);
        }
    }
    Json::Null
}
