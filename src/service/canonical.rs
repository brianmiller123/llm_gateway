//! Canonical JSON 序列化（键名递归排序），cc-switch `json_canonical.rs` 同款思想。
//!
//! 用途：工具调用参数、tool_result 内容对象在跨协议转换时以稳定形态序列化，
//! 避免相同语义的 JSON 因键序漂移破坏上游前缀缓存（prompt prefix cache）。

use serde_json::Value;

/// 任意 JSON 值 → canonical JSON 字符串（对象键按字典序递归排序）。
/// 数值/字符串/布尔/Null 原样；非对象数组按 serde 默认序列化。
pub fn canonical_json_string(v: &Value) -> String {
    serde_json::to_string(&canonical_value(v)).unwrap_or_else(|_| v.to_string())
}

/// JSON 字符串（工具参数等 wire 形态）→ canonical 化字符串。
/// 可解析则重排键序后回写；不可解析或非对象/数组则原样返回（宽容上游）。
pub fn canonicalize_json_string_if_parseable(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return s.to_string();
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(v) if v.is_object() || v.is_array() => serde_json::to_string(&canonical_value(&v))
            .unwrap_or_else(|_| s.to_string()),
        _ => s.to_string(),
    }
}

fn canonical_value(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            // BTreeMap 迭代天然按键排序；serde_json Map 的 sorted 形态即 BTreeMap
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                out.insert(k.clone(), canonical_value(&map[k]));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonical_value).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_keys_recursively() {
        let v = json!({"z": 1, "a": {"y": true, "b": [2, {"q": null, "c": 1}]}});
        assert_eq!(
            canonical_json_string(&v),
            r#"{"a":{"b":[2,{"c":1,"q":null}],"y":true},"z":1}"#
        );
    }

    #[test]
    fn canonicalize_tool_arguments() {
        assert_eq!(
            canonicalize_json_string_if_parseable(r#"{"city":"Paris","unit":"c"}"#),
            r#"{"city":"Paris","unit":"c"}"#
        );
        assert_eq!(
            canonicalize_json_string_if_parseable(r#"{"b":2,"a":1}"#),
            r#"{"a":1,"b":2}"#
        );
    }

    #[test]
    fn canonicalize_keeps_invalid_as_is() {
        assert_eq!(canonicalize_json_string_if_parseable(""), "");
        assert_eq!(canonicalize_json_string_if_parseable("partial{"), "partial{");
        assert_eq!(canonicalize_json_string_if_parseable("42"), "42");
        // 纯字符串标量不重排（非对象/数组）
        assert_eq!(canonicalize_json_string_if_parseable("\"x\""), "\"x\"");
    }
}
