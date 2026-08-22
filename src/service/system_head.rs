//! Chat 消息数组的 system 收拢（L1）。
//!
//! cc-switch `normalize_openai_system_messages`（transform.rs:308-357）移植：
//! 部分严格上游（MiniMax 等）只接受首条消息为 system，多条 system 或
//! system 出现在消息流中间会直接 400。启用时：
//! - 0 条 system → 无操作
//! - 1 条 system 不在头部 → 移到头部
//! - 多条 system → 全部文本（字符串 content 与 content part 数组的 text 段）
//!   按序拼接为单条头部 system 消息
//!
//! 由管理员按模型启用（model_routes.strict_system_head），默认关闭，
//! 不影响常规上游（多条 system 对 OpenAI 兼容上游是合法输入）。

use serde_json::{json, Value};

/// 将 `messages` 中全部 system 消息收拢为头部单条（多条时按序拼文本）。
/// 返回是否发生改写（供透传快路径决定是否需要重序列化）。
pub(crate) fn collect_system_to_head(messages: &mut Vec<Value>) -> bool {
    let system_count = messages
        .iter()
        .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("system"))
        .count();

    match system_count {
        0 => false,
        1 => {
            let Some(index) = messages.iter().position(|m| {
                m.get("role").and_then(|v| v.as_str()) == Some("system")
            }) else {
                return false;
            };
            if index > 0 {
                let message = messages.remove(index);
                messages.insert(0, message);
                true
            } else {
                false
            }
        }
        _ => {
            let mut parts = Vec::new();
            messages.retain(|m| {
                if m.get("role").and_then(|v| v.as_str()) != Some("system") {
                    return true;
                }
                match m.get("content") {
                    Some(Value::String(text)) if !text.is_empty() => parts.push(text.clone()),
                    Some(Value::Array(content_parts)) => {
                        let text = content_parts
                            .iter()
                            .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.is_empty() {
                            parts.push(text);
                        }
                    }
                    _ => {}
                }
                false
            });
            if !parts.is_empty() {
                messages.insert(0, json!({"role": "system", "content": parts.join("\n")}));
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_system_noop() {
        let mut msgs = vec![json!({"role": "user", "content": "hi"})];
        assert!(!collect_system_to_head(&mut msgs));
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn single_mid_system_moves_to_head() {
        let mut msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "system", "content": "rules"}),
            json!({"role": "assistant", "content": "ok"}),
        ];
        assert!(collect_system_to_head(&mut msgs));
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn single_head_system_untouched() {
        let mut msgs = vec![
            json!({"role": "system", "content": "rules"}),
            json!({"role": "user", "content": "hi"}),
        ];
        assert!(!collect_system_to_head(&mut msgs));
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn multiple_systems_merged_to_head_in_order() {
        let mut msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "system", "content": "rule A"}),
            json!({"role": "assistant", "content": "ok"}),
            json!({"role": "system", "content": "rule B"}),
        ];
        assert!(collect_system_to_head(&mut msgs));
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "rule A\nrule B");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
    }

    #[test]
    fn content_parts_texts_joined() {
        let mut msgs = vec![
            json!({"role": "system", "content": [{"type": "text", "text": "A"}, {"type": "text", "text": "B"}]}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "system", "content": "C"}),
        ];
        assert!(collect_system_to_head(&mut msgs));
        assert_eq!(msgs[0]["content"], "A\nB\nC");
        assert_eq!(msgs.len(), 2);
    }
}
