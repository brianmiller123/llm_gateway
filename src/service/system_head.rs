//! Chat 消息数组的 system 收拢（L1）。
//!
//! cc-switch `normalize_openai_system_messages`（transform.rs:308-357）移植：
//! 部分严格上游只接受首条消息为 system，多条 system 或 system 出现在消息流
//! 中间会直接 400（MiniMax「多条 system 400」、qwen3「system message must
//! be at the beginning」）。启用时：
//! - 0 条 system → 无操作
//! - 1 条 system 不在头部 → 移到头部
//! - 多条 system，merge = true → 全部文本（字符串 content 与 content part
//!   数组的 text 段）按序拼接为单条头部 system 消息（MiniMax 类只接受
//!   单条 system 的上游）
//! - 多条 system，merge = false → 按原序移到头部，保持多条独立消息
//!   （qwen3 类只要求 system 在开头、不限制条数的上游）
//!
//! 由管理员按模型启用（model_routes.strict_system_head +
//! model_routes.system_head_merge），默认关闭，不影响常规上游（多条
//! system 对 OpenAI 兼容上游是合法输入）。

use serde_json::{json, Value};

/// 将 `messages` 中全部 system 消息收拢到头部。
/// `merge`：多条 system 时是否拼接为单条（false = 保持多条独立、仅前移）。
/// 返回是否发生改写（供透传快路径决定是否需要重序列化）。
pub(crate) fn collect_system_to_head(messages: &mut Vec<Value>, merge: bool) -> bool {
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
            if !merge {
                // 仅前移：按原序把 system 消息稳定分区到头部，消息对象原样保留
                let (systems, rest): (Vec<Value>, Vec<Value>) = messages
                    .drain(..)
                    .partition(|m| m.get("role").and_then(|v| v.as_str()) == Some("system"));
                messages.extend(systems.into_iter().chain(rest));
                return true;
            }
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
        assert!(!collect_system_to_head(&mut msgs, true));
        assert!(!collect_system_to_head(&mut msgs, false));
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn single_mid_system_moves_to_head() {
        let mut msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "system", "content": "rules"}),
            json!({"role": "assistant", "content": "ok"}),
        ];
        assert!(collect_system_to_head(&mut msgs, true));
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");

        let mut msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "system", "content": "rules"}),
            json!({"role": "assistant", "content": "ok"}),
        ];
        assert!(collect_system_to_head(&mut msgs, false));
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
    }

    #[test]
    fn single_head_system_untouched() {
        let mut msgs = vec![
            json!({"role": "system", "content": "rules"}),
            json!({"role": "user", "content": "hi"}),
        ];
        assert!(!collect_system_to_head(&mut msgs, true));
        assert!(!collect_system_to_head(&mut msgs, false));
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
        assert!(collect_system_to_head(&mut msgs, true));
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
        assert!(collect_system_to_head(&mut msgs, true));
        assert_eq!(msgs[0]["content"], "A\nB\nC");
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn multiple_systems_kept_separate_in_original_order() {
        // merge = false：仅前移，多条 system 保持独立、按原序排头部，
        // 消息对象（name / content parts 等字段）原样保留
        let mut msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "system", "name": "persona", "content": "rule A"}),
            json!({"role": "assistant", "content": "ok"}),
            json!({"role": "system", "content": [{"type": "text", "text": "rule B"}]}),
        ];
        assert!(collect_system_to_head(&mut msgs, false));
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["name"], "persona");
        assert_eq!(msgs[0]["content"], "rule A");
        assert_eq!(msgs[1]["role"], "system");
        assert_eq!(msgs[1]["content"], json!([{"type": "text", "text": "rule B"}]));
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[3]["role"], "assistant");
    }

    #[test]
    fn separate_mode_already_head_multi_system_rewritten_flag() {
        // 两条 system 本就在头部但中间夹了 user —— 前移后顺序改变，仍算改写；
        // 反过来「全部 system 连续位于头部」时稳定分区结果与原序一致
        let mut msgs = vec![
            json!({"role": "system", "content": "A"}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "system", "content": "B"}),
            json!({"role": "assistant", "content": "ok"}),
        ];
        assert!(collect_system_to_head(&mut msgs, false));
        assert_eq!(msgs[0]["content"], "A");
        assert_eq!(msgs[1]["content"], "B");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[3]["role"], "assistant");

        let mut msgs = vec![
            json!({"role": "system", "content": "A"}),
            json!({"role": "system", "content": "B"}),
            json!({"role": "user", "content": "hi"}),
        ];
        // 稳定分区后序列不变，但保守返回 true（调用方最多多一次重序列化）
        assert!(collect_system_to_head(&mut msgs, false));
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["content"], "A");
        assert_eq!(msgs[1]["content"], "B");
        assert_eq!(msgs[2]["role"], "user");
    }
}
