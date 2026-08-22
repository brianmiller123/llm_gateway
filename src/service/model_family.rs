//! 模型族判定（cc-switch `providers/transform.rs:58-81` 移植）。
//!
//! 两个转换链共用：只有支持 `reasoning_effort` 的模型族才把推理力度意图
//! 转发为 Chat `reasoning_effort` 顶层字段 —— 向不认识该字段的严格上游
//! （vLLM / 企业网关）发送未知参数会直接 400，或被静默忽略造成行为偏差。

use serde_json::Value;

/// o 系列模型（o1/o3/o4-mini…，仅接受 max_completion_tokens / reasoning_effort）；
pub fn is_openai_o_series(model: &str) -> bool {
    let bare = model.rsplit('/').next().unwrap_or(model);
    bare.len() > 1
        && bare.starts_with('o')
        && bare.as_bytes().get(1).is_some_and(|b| b.is_ascii_digit())
}

/// P1-7：MiniMax 系模型（MiniMax / abab 命名族）。此类上游对多条 system
/// 消息直接 400，转换出站强制收拢 system 到头部（cc-switch 无条件收拢的
/// 具名动机；cc-switch transform_codex_chat.rs:554-580 同款）。
pub fn is_mini_max(model: &str) -> bool {
    let normalized = model.to_lowercase();
    normalized.contains("minimax") || normalized.contains("abab")
}

/// 是否支持 `reasoning_effort`（cc-switch supports_reasoning_effort 同款）：
/// - o 系列：o1、o3、o4-mini 等
/// - GPT-5+：gpt-5、gpt-5.1、gpt-5-codex 等
/// - xAI Grok Build：grok-4.5 / grok-build-*
pub fn supports_reasoning_effort(model: &str) -> bool {
    let normalized = model.to_lowercase();
    let bare = normalized.rsplit('/').next().unwrap_or(&normalized);
    is_openai_o_series(bare)
        || bare
            .strip_prefix("gpt-")
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| c.is_ascii_digit() && c >= '5')
        || bare == "grok-4.5"
        || bare.starts_with("grok-4.5-")
        || bare.starts_with("grok-build-")
}

/// effort 值是否为「显式关闭推理」（cc-switch reasoning_requested :451-460 同款）。
/// OpenAI `reasoning_effort` 枚举不含 none，显式关闭时不发字段（默认即关闭）。
pub fn effort_is_off(effort: &str) -> bool {
    matches!(effort.trim().to_ascii_lowercase().as_str(), "none" | "off" | "disabled")
}

/// 路由级 `reasoning_effort_mode`（H3，cc-switch effortValueMode 子集）：
/// 出站 `reasoning_effort` 的值域钳制/形态改写。默认 passthrough = 原值透传。
/// - deepseek：max/xhigh/ultra → max；其余已知档 → high（DeepSeek 仅两档）
/// - low_high：minimal/low → low；其余 → high（仅两档的上游）
/// - openrouter：max/xhigh/ultra → xhigh；合法值透传；未知值丢弃
///   （OpenRouter 枚举无 max，400 风险；cc-switch openclaw#77350 同款）
/// - zen：合法档位逐模型（effort_levels 表）；无表 → None 完全不发；
///   有表 → 钳到「不小于请求的最近合法档」，请求超出最高档则取最高合法档
///   （cc-switch map_reasoning_effort "zen" transform_codex_chat.rs:504-521 同款）
/// 未知值在 passthrough 透传（网关无从判定目标枚举），在具名模式丢弃。
/// 兼容入口（无档位表；测试与外部调用用）
#[allow(dead_code)]
pub fn clamp_reasoning_effort(effort: &str, mode: &str) -> Option<&'static str> {
    clamp_reasoning_effort_for(effort, mode, None)
}

/// P1-10：zen 模式带逐模型档位表版本。
/// `levels`：模型允许的档位列表（顺序无关，如 ["high","max"]）；None = 无表。
/// 其他模式忽略 levels。返回值与 levels 同生命周期。
pub fn clamp_reasoning_effort_for<'a>(
    effort: &str,
    mode: &str,
    levels: Option<&'a [String]>,
) -> Option<&'a str> {
    let effort = effort.trim().to_ascii_lowercase();
    if effort_is_off(&effort) {
        return None;
    }
    match mode {
        "deepseek" => match effort.as_str() {
            "max" | "xhigh" | "ultra" => Some("max"),
            "minimal" | "low" | "medium" | "high" => Some("high"),
            _ => None,
        },
        "low_high" => match effort.as_str() {
            "minimal" | "low" => Some("low"),
            "medium" | "high" | "xhigh" | "max" | "ultra" => Some("high"),
            _ => None,
        },
        "openrouter" => match effort.as_str() {
            "max" | "xhigh" | "ultra" => Some("xhigh"),
            "high" => Some("high"),
            "medium" => Some("medium"),
            "low" => Some("low"),
            "minimal" => Some("minimal"),
            _ => None,
        },
        "zen" => {
            let levels = levels?;
            let requested = zen_effort_rank(&effort)?;
            levels
                .iter()
                .filter_map(|level| zen_effort_rank(level).map(|rank| (rank, level.as_str())))
                .filter(|(rank, _)| *rank >= requested)
                .min_by_key(|(rank, _)| *rank)
                .or_else(|| {
                    levels
                        .iter()
                        .filter_map(|level| zen_effort_rank(level).map(|rank| (rank, level.as_str())))
                        .max_by_key(|(rank, _)| *rank)
                })
                .map(|(_, level)| level)
        }
        // passthrough 及未知模式：已知扩展档位透传，其余丢弃（防拼写错误直透上游）
        _ => match effort.as_str() {
            "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra" => {
                Some(match effort.as_str() {
                    "minimal" => "minimal",
                    "low" => "low",
                    "medium" => "medium",
                    "high" => "high",
                    "xhigh" => "xhigh",
                    "max" => "max",
                    _ => "ultra",
                })
            }
            _ => None,
        },
    }
}

/// P1-10：zen 档位表（GATEWAY_ZEN_EFFORT_TABLE）——`模型前缀=档位,档位,...;...`
/// 镜像 cc-switch per-model reasoningLevels 目录（models.dev）。最长前缀命中；
/// 无命中 → None（该模型未收录 → 完全不发 reasoning_effort，cc-switch 同款）。
fn zen_effort_levels_for(model: Option<&str>) -> Option<&'static [String]> {
    static TABLE: std::sync::LazyLock<Vec<(String, Vec<String>)>> =
        std::sync::LazyLock::new(|| {
            std::env::var("GATEWAY_ZEN_EFFORT_TABLE")
                .unwrap_or_default()
                .split(';')
                .filter(|s| !s.trim().is_empty())
                .filter_map(|entry| {
                    let (pattern, levels) = entry.split_once('=')?;
                    let levels: Vec<String> = levels
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    (!levels.is_empty())
                        .then(|| (pattern.trim().to_ascii_lowercase(), levels))
                })
                .collect()
        });
    let Some(model) = model.map(str::to_ascii_lowercase) else {
        return None;
    };
    TABLE
        .iter()
        .filter(|(p, _)| model.starts_with(p))
        .max_by_key(|(p, _)| p.len())
        .map(|(_, levels)| levels.as_slice())
}

/// P1-10：Codex 规范档位序（minimal < low < medium < high < xhigh < max < ultra），
/// 供 zen 逐模型钳制做大小比较；目录里的非法/扩展值（如 "none"）返回 None
///（cc-switch zen_effort_rank transform_codex_chat.rs:537-548 同款）
pub fn zen_effort_rank(effort: &str) -> Option<u8> {
    match effort.trim().to_ascii_lowercase().as_str() {
        "minimal" => Some(0),
        "low" => Some(1),
        "medium" => Some(2),
        "high" => Some(3),
        "xhigh" => Some(4),
        "max" => Some(5),
        "ultra" => Some(6),
        _ => None,
    }
}


/// H3：对已构建的出站 Chat 请求体应用路由级 effort 钳制模式。
/// - 显式关闭（none/off/disabled）：openrouter → 改写为 `reasoning: {effort: "none"}`
///   忠实转发（M9）；其余模式一律移除字段（OpenAI 枚举不含 none，直发 400）
/// - 无 mode / passthrough：已知档位透传（none 除外）
/// - deepseek / low_high：reasoning_effort 值域钳制（无效值移除字段）
/// - openrouter：顶层 reasoning_effort 改写为 `reasoning: {effort}` 对象形态
/// 兼容入口（无 gating model；测试与外部调用用）
#[allow(dead_code)]
pub fn apply_reasoning_effort_mode(body: &mut Value, mode: &str) {
    apply_reasoning_effort_mode_for_model(body, mode, None);
}

/// P1-10：带 gating model 的版本——zen 模式按模型查档位表
///（GATEWAY_ZEN_EFFORT_TABLE；无表 → 完全不发 reasoning_effort）
pub fn apply_reasoning_effort_mode_for_model(body: &mut Value, mode: &str, gating_model: Option<&str>) {
    let mode = mode.trim();
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let Some(effort) = obj
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return;
    };
    if effort_is_off(&effort) {
        obj.remove("reasoning_effort");
        if mode == "openrouter" {
            obj.insert("reasoning".into(), serde_json::json!({"effort": "none"}));
        }
        return;
    }
    if mode.is_empty() || mode == "passthrough" {
        return;
    }
    let levels = if mode == "zen" {
        zen_effort_levels_for(gating_model)
    } else {
        None
    };
    let Some(clamped) = clamp_reasoning_effort_for(&effort, mode, levels) else {
        obj.remove("reasoning_effort");
        return;
    };
    if mode == "openrouter" {
        obj.remove("reasoning_effort");
        obj.insert("reasoning".into(), serde_json::json!({"effort": clamped}));
    } else {
        obj.insert("reasoning_effort".into(), serde_json::Value::String(clamped.to_string()));
    }
}

/// H3：thinking 形态配置（cc-switch `CodexChatReasoningConfig` thinking_param 子集）。
/// 对已构建的出站 Chat 请求体应用路由级 thinking 形态：
/// - None/空："无 thinking 形态"——剥离 thinking / enable_thinking / thinking_budget /
///   reasoning_split 全部形态字段，仅保留 reasoning_effort 渠道（默认，防严格
///   上游对未知字段 400）
/// - thinking_param：`{"thinking": {"type": "enabled"}}`（要求 thinking 对象形态的上游）
/// - reasoning_split：`{"reasoning_split": true}`（国产推理网关拆分输出形态）
/// - enable_thinking：`{"enable_thinking": true}`（DeepSeek Responses 方言），
///   客户端原带 thinking_budget 时一并透传
///
/// 仅在存在推理意图（reasoning_effort 非关闭，或客户端显式 enable_thinking）时
/// 产出形态字段——无意图的普通请求不被注入 thinking 开关。
pub fn apply_thinking_form(body: &mut Value, form: Option<&str>) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let client_thinking_enabled = obj
        .get("enable_thinking")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let client_budget = obj.get("thinking_budget").cloned();
    let effort_intent = obj
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .map(|e| !effort_is_off(e))
        .unwrap_or(false);
    // 先剥离全部 thinking 形态字段（后续按形态重新产出）
    obj.remove("thinking");
    obj.remove("enable_thinking");
    obj.remove("thinking_budget");
    obj.remove("reasoning_split");
    let Some(form) = form.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    let intent = effort_intent || client_thinking_enabled;
    if !intent {
        return;
    }
    match form {
        "thinking_param" => {
            obj.insert("thinking".into(), serde_json::json!({"type": "enabled"}));
        }
        "reasoning_split" => {
            obj.insert("reasoning_split".into(), serde_json::json!(true));
        }
        "enable_thinking" => {
            obj.insert("enable_thinking".into(), serde_json::json!(true));
            if let Some(budget) = client_budget {
                obj.insert("thinking_budget".into(), budget);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn o_series_detection() {
        assert!(is_openai_o_series("o1"));
        assert!(is_openai_o_series("o3-mini"));
        assert!(is_openai_o_series("openai/o4-mini"));
        assert!(!is_openai_o_series("openai/gpt-4o")); // 'g' 非 'o' 开头
        assert!(!is_openai_o_series("omni")); // 第二字符非数字
    }

    #[test]
    fn reasoning_effort_support() {
        assert!(supports_reasoning_effort("o3-mini"));
        assert!(supports_reasoning_effort("gpt-5"));
        assert!(supports_reasoning_effort("gpt-5.1-codex"));
        assert!(supports_reasoning_effort("grok-4.5"));
        assert!(!supports_reasoning_effort("gpt-4o"));
        assert!(!supports_reasoning_effort("deepseek-chat"));
        assert!(!supports_reasoning_effort("qwen-max"));
    }

    #[test]
    fn off_values() {
        assert!(effort_is_off("none"));
        assert!(effort_is_off(" OFF "));
        assert!(effort_is_off("disabled"));
        assert!(!effort_is_off("low"));
        assert!(!effort_is_off(""));
    }

    /// H3：effort 值域钳制（deepseek/low_high/openrouter/passthrough）
    #[test]
    fn effort_clamp_modes() {
        // deepseek：扩展高档 → max；普通档 → high；未知丢弃
        assert_eq!(clamp_reasoning_effort("xhigh", "deepseek"), Some("max"));
        assert_eq!(clamp_reasoning_effort("MAX", "deepseek"), Some("max"));
        assert_eq!(clamp_reasoning_effort("ultra", "deepseek"), Some("max"));
        assert_eq!(clamp_reasoning_effort("low", "deepseek"), Some("high"));
        assert_eq!(clamp_reasoning_effort("bogus", "deepseek"), None);
        // low_high：两档
        assert_eq!(clamp_reasoning_effort("minimal", "low_high"), Some("low"));
        assert_eq!(clamp_reasoning_effort("medium", "low_high"), Some("high"));
        // openrouter：max 非法 → xhigh；未知丢弃
        assert_eq!(clamp_reasoning_effort("max", "openrouter"), Some("xhigh"));
        assert_eq!(clamp_reasoning_effort("medium", "openrouter"), Some("medium"));
        assert_eq!(clamp_reasoning_effort("ultra", "openrouter"), Some("xhigh"));
        assert_eq!(clamp_reasoning_effort("bogus", "openrouter"), None);
        // passthrough：已知枚举透传、未知丢弃、显式关闭 → None
        assert_eq!(clamp_reasoning_effort("xhigh", "passthrough"), Some("xhigh"));
        assert_eq!(clamp_reasoning_effort("none", "passthrough"), None);
        assert_eq!(clamp_reasoning_effort("bogus", "passthrough"), None);
    }

    /// H3：出站体 effort 钳制/形态改写
    #[test]
    fn apply_effort_mode_rewrites_body() {
        let mut body = serde_json::json!({"model": "m", "reasoning_effort": "max"});
        // passthrough / 空：不动
        apply_reasoning_effort_mode(&mut body, "passthrough");
        assert_eq!(body["reasoning_effort"], "max");
        apply_reasoning_effort_mode(&mut body, "");
        assert_eq!(body["reasoning_effort"], "max");
        // openrouter：改写为 reasoning.effort 对象，且 max → xhigh
        apply_reasoning_effort_mode(&mut body, "openrouter");
        assert!(body.get("reasoning_effort").is_none());
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        // deepseek：xhigh → max
        let mut b2 = serde_json::json!({"reasoning_effort": "xhigh"});
        apply_reasoning_effort_mode(&mut b2, "deepseek");
        assert_eq!(b2["reasoning_effort"], "max");
        // 无效值：字段移除
        let mut b3 = serde_json::json!({"reasoning_effort": "bogus"});
        apply_reasoning_effort_mode(&mut b3, "deepseek");
        assert!(b3.get("reasoning_effort").is_none());
        // 无字段：不动
        let mut b4 = serde_json::json!({"model": "m"});
        apply_reasoning_effort_mode(&mut b4, "openrouter");
        assert_eq!(b4, serde_json::json!({"model": "m"}));
    }
    /// H3：thinking 形态——无形态剥离、三种形态按推理意图产出
    #[test]
    fn apply_thinking_form_shapes_output() {
        // None：剥离全部 thinking 形态字段
        let mut b = serde_json::json!({
            "reasoning_effort": "high",
            "enable_thinking": true,
            "thinking_budget": 8192,
            "reasoning_split": true,
            "thinking": {"type": "enabled"}
        });
        apply_thinking_form(&mut b, None);
        assert!(b.get("enable_thinking").is_none());
        assert!(b.get("thinking_budget").is_none());
        assert!(b.get("reasoning_split").is_none());
        assert!(b.get("thinking").is_none());
        assert_eq!(b["reasoning_effort"], "high", "effort 渠道保留");
        // thinking_param：有意图 → thinking 对象
        apply_thinking_form(&mut b, Some("thinking_param"));
        assert_eq!(b["thinking"], serde_json::json!({"type": "enabled"}));
        // reasoning_split
        let mut b2 = serde_json::json!({"reasoning_effort": "high"});
        apply_thinking_form(&mut b2, Some("reasoning_split"));
        assert_eq!(b2["reasoning_split"], serde_json::json!(true));
        assert!(b2.get("reasoning_effort").is_some(), "effort 不动");
        // enable_thinking：客户端 budget 透传
        let mut b3 = serde_json::json!({"reasoning_effort": "high", "thinking_budget": 8192});
        apply_thinking_form(&mut b3, Some("enable_thinking"));
        assert_eq!(b3["enable_thinking"], serde_json::json!(true));
        assert_eq!(b3["thinking_budget"], serde_json::json!(8192));
        // 无推理意图：不产出形态字段（客户端 enable_thinking 显式开启除外）
        let mut b4 = serde_json::json!({"temperature": 0.7});
        apply_thinking_form(&mut b4, Some("thinking_param"));
        assert!(b4.get("thinking").is_none());
        let mut b5 = serde_json::json!({"enable_thinking": true});
        apply_thinking_form(&mut b5, Some("reasoning_split"));
        assert_eq!(b5["reasoning_split"], serde_json::json!(true), "客户端显式意图");
        // effort 显式关闭：不产出
        let mut b6 = serde_json::json!({"reasoning_effort": "none"});
        apply_thinking_form(&mut b6, Some("thinking_param"));
        assert!(b6.get("thinking").is_none());
    }

    /// P1-10：zen 逐模型档位钳制（cc-switch map_reasoning_effort zen 同款语义）
    #[test]
    fn zen_effort_clamp_per_model_levels() {
        let levels: Vec<String> = ["low", "high", "max"].iter().map(|s| s.to_string()).collect();
        // 请求档 <= 最高合法档 → 向上钳到最近合法档
        assert_eq!(
            clamp_reasoning_effort_for("medium", "zen", Some(&levels)),
            Some("high")
        );
        assert_eq!(
            clamp_reasoning_effort_for("high", "zen", Some(&levels)),
            Some("high")
        );
        // 请求超出最高档 → 取最高合法档
        assert_eq!(
            clamp_reasoning_effort_for("ultra", "zen", Some(&levels)),
            Some("max")
        );
        // 无表 → 不发
        assert_eq!(
            clamp_reasoning_effort_for("high", "zen", None),
            None
        );
        // 请求值无法识别 → None
        assert_eq!(
            clamp_reasoning_effort_for("bogus", "zen", Some(&levels)),
            None
        );
        // 单档模型（kimi-k3 仅 max）
        let single: Vec<String> = ["max"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            clamp_reasoning_effort_for("low", "zen", Some(&single)),
            Some("max")
        );
        // 档位序：minimal < low < medium < high < xhigh < max < ultra
        assert_eq!(zen_effort_rank("minimal"), Some(0));
        assert_eq!(zen_effort_rank("ULTRA"), Some(6));
        assert_eq!(zen_effort_rank("none"), None);
    }
}
