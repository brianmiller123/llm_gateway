//! P0-4：Responses 请求的工具上下文（chat 名 ↔ 原始 spec 映射）。
//!
//! cc-switch `CodexToolContext`（transform_codex_chat.rs:59-254）同款语义：
//! - namespace 包装工具（`{type:"namespace", name, tools:[…]}`）拍平为
//!   `ns__name` chat 名（>64 字符截断 + 短哈希后缀，双向一致）
//! - `tool_search` 工具注入代理 function（name=`tool_search`）；上游同名调用
//!   映回 `tool_search_call` item
//! - input 里的 `tool_search_output` item 携带动态加载的工具（namespace 包装
//!   形态），递归注册进上下文
//! - custom 工具 / web_search 桥接标记沿用（替代原 request_custom_tool_names /
//!   request_bridges_web_search 两个独立通道）
//!
//! 请求侧（convert_req）用 `chat_name_for` 拍平历史 function_call 与 tool_choice；
//! 响应侧（convert_resp / stream）用 `lookup` 还原 namespace / tool_search_call /
//! custom_tool_call item。

use std::collections::HashMap;

use serde_json::Value;

/// chat 工具名上限（Chat 协议惯例；超长截断 + 短哈希后缀保持双向可还原）
const CHAT_TOOL_NAME_MAX_LEN: usize = 64;
/// tool_search 代理工具的 chat 名（cc-switch TOOL_SEARCH_PROXY_NAME 同款）
pub const TOOL_SEARCH_PROXY_NAME: &str = "tool_search";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// 普通 function 工具（chat 名 == 原名）
    Function,
    /// custom 工具（降级为 function，响应侧还原 custom_tool_call）
    Custom,
    /// namespace 包装的 function 工具（chat 名为拍平形态）
    Namespace,
    /// tool_search 代理工具（响应侧还原 tool_search_call）
    ToolSearch,
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub kind: ToolKind,
    /// 原始工具名（namespace 工具为 namespace 内的短名）
    pub name: String,
    /// namespace 工具的命名空间（其余 None）
    pub namespace: Option<String>,
}

/// 请求工具上下文：chat 名 → spec；响应侧据此还原 item 形态。
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    specs: HashMap<String, ToolSpec>,
    /// (namespace, name) → chat 名（历史 function_call item 带 namespace 字段时
    /// 直接查表；未注册时按确定性拍平规则即时生成，保证请求/响应两侧一致）
    ns_lookup: HashMap<(String, String), String>,
    /// 请求声明了 hosted web_search（桥接标记，原 request_bridges_web_search）
    pub bridges_web_search: bool,
}

impl ToolContext {
    /// 从 Responses 请求构建：tools 数组 + input 内 tool_search_output 的动态工具。
    pub fn from_request(req: &Value) -> Self {
        let mut ctx = Self::default();
        if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
            for tool in tools {
                ctx.add_response_tool(tool);
            }
        }
        if let Some(input) = req.get("input") {
            collect_tool_search_output_tools(input, &mut ctx);
        }
        ctx
    }

    pub fn lookup(&self, chat_name: &str) -> Option<&ToolSpec> {
        self.specs.get(chat_name)
    }

    /// M6：chat 名是否为请求声明的 custom 工具（还原 custom_tool_call item）
    #[allow(dead_code)]
    pub fn is_custom(&self, chat_name: &str) -> bool {
        self.lookup(chat_name)
            .is_some_and(|s| s.kind == ToolKind::Custom)
    }

    /// 响应/历史侧：原始名（+可选 namespace）→ chat 名。
    /// 已注册用注册名；未注册的 namespace 组合按确定性规则即时拍平。
    pub fn chat_name_for(&self, name: &str, namespace: Option<&str>) -> String {
        match namespace.filter(|v| !v.is_empty()) {
            Some(ns) => self
                .ns_lookup
                .get(&(ns.to_string(), name.to_string()))
                .cloned()
                .unwrap_or_else(|| flatten_namespace_tool_name(ns, name)),
            None => name.to_string(),
        }
    }

    fn add_chat_tool(&mut self, chat_name: String, spec: ToolSpec) {
        if chat_name.trim().is_empty() || self.specs.contains_key(&chat_name) {
            return;
        }
        if let Some(ns) = spec.namespace.clone() {
            self.ns_lookup
                .insert((ns, spec.name.clone()), chat_name.clone());
        }
        self.specs.insert(chat_name, spec);
    }

    fn add_function_tool(&mut self, tool: &Value, namespace: Option<&str>) {
        let Some(original_name) = responses_tool_name(tool) else {
            return;
        };
        let chat_name = namespace
            .map(|ns| flatten_namespace_tool_name(ns, &original_name))
            .unwrap_or_else(|| original_name.clone());
        self.add_chat_tool(
            chat_name,
            ToolSpec {
                kind: if namespace.is_some() {
                    ToolKind::Namespace
                } else {
                    ToolKind::Function
                },
                name: original_name,
                namespace: namespace.map(str::to_string),
            },
        );
    }

    fn add_custom_tool(&mut self, name: &str) {
        self.add_chat_tool(
            name.to_string(),
            ToolSpec {
                kind: ToolKind::Custom,
                name: name.to_string(),
                namespace: None,
            },
        );
    }

    fn add_tool_search_tool(&mut self) {
        self.add_chat_tool(
            TOOL_SEARCH_PROXY_NAME.to_string(),
            ToolSpec {
                kind: ToolKind::ToolSearch,
                name: TOOL_SEARCH_PROXY_NAME.to_string(),
                namespace: None,
            },
        );
    }

    fn add_namespace_tool(&mut self, namespace_tool: &Value) {
        let Some(namespace) = namespace_tool.get("name").and_then(|v| v.as_str()) else {
            return;
        };
        let Some(children) = namespace_tool
            .get("tools")
            .or_else(|| namespace_tool.get("children"))
            .and_then(|v| v.as_array())
        else {
            return;
        };
        for child in children {
            if child.get("type").and_then(|v| v.as_str()) == Some("function") {
                self.add_function_tool(child, Some(namespace));
            }
        }
    }

    fn add_response_tool(&mut self, tool: &Value) {
        match tool {
            Value::String(name) => {
                let name = name.trim();
                if !name.is_empty() {
                    self.add_custom_tool(name);
                }
            }
            Value::Object(_) => match tool.get("type").and_then(|v| v.as_str()) {
                Some("function") => self.add_function_tool(tool, None),
                Some("custom") => {
                    if let Some(name) = responses_tool_name(tool) {
                        self.add_custom_tool(&name);
                    }
                }
                Some("tool_search") => self.add_tool_search_tool(),
                Some("namespace") => self.add_namespace_tool(tool),
                // 四-4：hosted web_search 声明 → 桥接标记（上游同名 function
                // 调用映回 web_search_call item）
                Some("web_search") => self.bridges_web_search = true,
                _ => {}
            },
            _ => {}
        }
    }
}

/// tool 名提取（嵌套 function.name 优先，回退顶层 name；cc-switch responses_tool_name）
fn responses_tool_name(tool: &Value) -> Option<String> {
    tool.get("function")
        .and_then(|f| f.get("name"))
        .or_else(|| tool.get("name"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// `ns__name` 拍平；超 64 字符截断 + sha256 短哈希后缀
///（cc-switch flatten_namespace_tool_name :1223-1240 同款，确定性双向一致）
pub fn flatten_namespace_tool_name(namespace: &str, name: &str) -> String {
    let full_name = format!("{namespace}__{name}");
    if full_name.len() <= CHAT_TOOL_NAME_MAX_LEN {
        return full_name;
    }
    let hash = short_sha256_hex(full_name.as_bytes());
    let suffix = format!("__{hash}");
    let prefix_len = CHAT_TOOL_NAME_MAX_LEN.saturating_sub(suffix.len());
    let mut prefix = String::new();
    for ch in full_name.chars() {
        if prefix.len() + ch.len_utf8() > prefix_len {
            break;
        }
        prefix.push(ch);
    }
    format!("{prefix}{suffix}")
}

fn short_sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// 递归收集 input 内 tool_search_output item 携带的动态工具
///（cc-switch collect_tool_search_output_tools :1200-1221 同款）
fn collect_tool_search_output_tools(value: &Value, ctx: &mut ToolContext) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_tool_search_output_tools(item, ctx);
            }
        }
        Value::Object(obj) => {
            if obj.get("type").and_then(|v| v.as_str()) == Some("tool_search_output") {
                if let Some(tools) = obj.get("tools").and_then(|v| v.as_array()) {
                    for tool in tools {
                        ctx.add_response_tool(tool);
                    }
                }
            }
            for v in obj.values() {
                collect_tool_search_output_tools(v, ctx);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flattens_namespace_tools() {
        let ctx = ToolContext::from_request(&json!({
            "tools": [
                {"type": "namespace", "name": "mcp__server", "tools": [
                    {"type": "function", "name": "query", "parameters": {"type": "object"}}
                ]},
                {"type": "function", "name": "plain"},
                {"type": "custom", "name": "apply_patch"},
                {"type": "tool_search"}
            ]
        }));
        let spec = ctx
            .lookup("mcp__server__query")
            .expect("flattened name registered");
        assert_eq!(spec.kind, ToolKind::Namespace);
        assert_eq!(spec.name, "query");
        assert_eq!(spec.namespace.as_deref(), Some("mcp__server"));
        assert!(
            ctx.lookup("plain")
                .is_some_and(|s| s.kind == ToolKind::Function)
        );
        assert!(ctx.is_custom("apply_patch"));
        assert!(
            ctx.lookup("tool_search")
                .is_some_and(|s| s.kind == ToolKind::ToolSearch)
        );
        // 历史侧即时拍平与注册名一致
        assert_eq!(
            ctx.chat_name_for("query", Some("mcp__server")),
            "mcp__server__query"
        );
    }

    #[test]
    fn long_names_get_hash_suffix_deterministically() {
        let ns = "n".repeat(40);
        let name = "t".repeat(40);
        let flat = flatten_namespace_tool_name(&ns, &name);
        assert!(flat.len() <= CHAT_TOOL_NAME_MAX_LEN);
        assert_eq!(flat, flatten_namespace_tool_name(&ns, &name));
        // 未注册组合同样可拍平
        let ctx = ToolContext::default();
        assert_eq!(ctx.chat_name_for(&name, Some(&ns)), flat);
    }

    #[test]
    fn collects_dynamic_tools_from_tool_search_output() {
        let ctx = ToolContext::from_request(&json!({
            "tools": [{"type": "tool_search"}],
            "input": [
                {"type": "tool_search_output", "call_id": "c1", "tools": [
                    {"type": "namespace", "name": "gmail", "tools": [
                        {"type": "function", "name": "search"}
                    ]}
                ]}
            ]
        }));
        assert!(ctx.lookup("gmail__search").is_some());
    }
}
