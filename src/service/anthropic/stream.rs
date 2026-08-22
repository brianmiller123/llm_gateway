//! Chat Completions 流式响应 → Anthropic Messages SSE（cc-switch `streaming.rs` 移植）。
//!
//! 事件生命周期：
//! `message_start` → `content_block_start`(thinking/text/tool_use) →
//! `content_block_delta`(thinking_delta/text_delta/input_json_delta) →
//! `content_block_stop`（逐块）→ `message_delta`(stop_reason+usage) → `message_stop`
//!
//! 防御语义（cc-switch 同款）：
//! - 工具块延迟启动：id+name 到齐才发 content_block_start，参数先缓冲
//! - 多 finish_reason chunk 去重（kimi-k2.6 等）：message_delta 只发一次，
//!   且缓存到 [DONE] 再发（保证 usage 完整）
//! - 上游流内错误帧 / 传输错误 → `event: error` + 抑制 message_delta/message_stop
//!   （不把失败伪装成成功）
//! - 断流兜底：已有输出 → 正常收尾（stop_reason 缺省 max_tokens）；
//!   零输出 → error 事件

use std::collections::BTreeMap;
use std::error::Error;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::stream::{once, Stream, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::state::AppState;
use crate::store::usage::UsageMeta;

use super::super::responses::dto::{
    delta_content_text, ChatStreamChunk, ToolCallDelta, Usage as DtoUsage,
};
use super::super::responses::stream::{ChatSseParser, SseEvent};
use super::convert_resp::{anthropic_usage, stop_reason_from_finish};

/// 单个工具块（按 Chat index 关联；started = content_block_start 已发）
#[derive(Debug, Default)]
struct ToolBlock {
    anthropic_index: i64,
    id: String,
    name: String,
    arguments: String,
    pending_args: String,
    started: bool,
    stopped: bool,
    /// 连续空白计数 — 检测上游无限空白 bug（Copilot 类缺陷，cc-switch 同款）
    consecutive_whitespace: usize,
    /// 已因无限空白被中止：不再下发该块的参数增量
    aborted: bool,
}

/// 非工具内容块类型（M16 单槽切换）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NonToolKind {
    Thinking,
    Text,
}

/// 当前打开的非工具块（M16：任意时刻最多一个；通道切换时先发
/// content_block_stop 再开新块，cc-switch "当前非工具块"单槽同款）
#[derive(Debug, Clone, Copy)]
struct NonToolBlock {
    kind: NonToolKind,
    index: i64,
}

impl NonToolBlock {
    fn stop_event(self) -> String {
        block_stop_event(self.index)
    }
}


/// 无限空白 bug 的连续空白字符阈值（cc-switch INFINITE_WHITESPACE_THRESHOLD）
const INFINITE_WHITESPACE_THRESHOLD: usize = 500;

/// Chat 流 → Anthropic 流 状态机
pub struct ChatToAnthropicState {
    id: String,
    model: String,
    usage: Option<DtoUsage>,
    /// 上游流内错误帧 / 传输错误 → 发 error 事件并抑制正常收尾
    failed: Option<String>,
    /// 当前打开的非工具块（M16：thinking/text 单槽，通道切换先关旧块再开新块）
    open_non_tool: Option<NonToolBlock>,
    sent_message_start: bool,
    /// message_delta 是否已发（Anthropic 协议只允许一个；部分上游重复发 finish_reason）
    has_emitted_message_delta: bool,
    pending_stop_reason: Option<String>,
    has_tool_use: bool,
    /// 因缺函数名被丢弃的工具调用数（cc-switch #4341 护栏，防静默空成功）
    dropped_tools: usize,
    finalized: bool,
    next_block_index: i64,
    thinking_block_index: i64,
    thinking_started: bool,
    thinking_stopped: bool,
    text_block_index: i64,
    text_started: bool,
    text_stopped: bool,
    /// 非工具块按真实打开顺序记录（M16：关闭顺序与到达序一致，不再固定
    /// thinking→text 倒挂）
    non_tool_open_order: Vec<NonToolKind>,
    tools: BTreeMap<i64, ToolBlock>,
    last_tool_key: Option<i64>,
    /// 内联 <think> 分离器（M4：思考混在 content 里时分离为 thinking 块）
    inline_think: crate::service::inline_think::InlineThinkState,
}

impl ChatToAnthropicState {
    pub fn new(id: String, model: String) -> Self {
        Self {
            id,
            model,
            usage: None,
            failed: None,
            open_non_tool: None,
            sent_message_start: false,
            has_emitted_message_delta: false,
            pending_stop_reason: None,
            has_tool_use: false,
            dropped_tools: 0,
            finalized: false,
            next_block_index: 0,
            thinking_block_index: 0,
            thinking_started: false,
            thinking_stopped: false,
            text_block_index: 0,
            text_started: false,
            text_stopped: false,
            non_tool_open_order: Vec::new(),
            tools: BTreeMap::new(),
            last_tool_key: None,
            inline_think: crate::service::inline_think::InlineThinkState::new(),
        }
    }

    /// H1：上游传输错误（流 Err 项）→ `event: error` 并抑制正常收尾。
    /// 客户端收到结构化错误而非 TCP 截断（此前 Err 直通 hyper 掐断连接；
    /// cc-switch streaming.rs:627-642 同款）。
    pub fn transport_error(&mut self, message: &str) -> Vec<String> {
        if self.failed.is_some() {
            return Vec::new();
        }
        let event = self.error_event(message);
        self.failed = Some(message.to_string());
        vec![event]
    }

    /// 消费一个 Chat chunk，产出 0..n 个 Anthropic SSE 帧字符串
    pub fn process_chunk(&mut self, chunk: &ChatStreamChunk) -> Vec<String> {
        if self.failed.is_some() {
            return Vec::new();
        }
        if let Some(err) = &chunk.error {
            let message = super::extract_stream_error_message(err);
            let event = self.error_event(&message);
            self.failed = Some(message);
            return vec![event];
        }
        if self.id.is_empty() && !chunk.id.is_empty() {
            self.id = chunk.id.clone();
        }
        if self.model.is_empty() && !chunk.model.is_empty() {
            self.model = chunk.model.clone();
        }
        if let Some(usage) = &chunk.usage {
            self.usage = Some(usage.clone());
        }

        let mut events = Vec::new();
        if !self.sent_message_start {
            self.sent_message_start = true;
            events.push(self.message_start_event());
        }
        for choice in &chunk.choices {
            if let Some(r) = choice
                .delta
                .reasoning_content
                .as_deref()
                .or(choice.delta.reasoning.as_deref())
                .filter(|r| !r.is_empty())
            {
                events.extend(self.append_thinking_delta(r));
            }
            if let Some(content) = choice
                .delta
                .content
                .as_ref()
                .and_then(delta_content_text)
                .filter(|c| !c.is_empty())
            {
                // M4：流首 <think>…</think> 块分离为 thinking 块，其余为 text
                let (think, text) = self.inline_think.feed(&content);
                for r in think {
                    events.extend(self.append_thinking_delta(&r));
                }
                for t in text {
                    events.extend(self.append_text_delta(&t));
                }
            }
            // 四-3：流式 refusal → text 块（非流式已有 refusal→text 兜底；
            // cc-switch 双方均缺此通道，拒答内容此前被静默丢弃）
            if let Some(r) = choice
                .delta
                .refusal
                .as_deref()
                .map(str::trim)
                .filter(|r| !r.is_empty())
            {
                events.extend(self.append_text_delta(r));
            }
            for tool_call in &choice.delta.tool_calls {
                events.extend(self.append_tool_delta(tool_call));
            }
            if let Some(fr) = choice
                .finish_reason
                .as_deref()
                .map(str::trim)
                .filter(|f| !f.is_empty())
            {
                // 后到的 finish_reason 覆盖（部分上游多 finish chunk，真实终态在最后；
                // message_delta 收尾时只发一次 —— 天然去重，cc-switch 同款）
                self.pending_stop_reason =
                    Some(stop_reason_from_finish(fr, self.has_tool_use));
            }
        }
        events
    }

    /// [DONE] 正常结束：关块 → message_delta(usage) → message_stop
    pub fn finalize(&mut self) -> Vec<String> {
        self.finalize_inner(false)
    }

    /// 断流兜底：已有输出正常收尾（stop_reason 缺省 max_tokens）；零输出 → error 事件
    pub fn finalize_truncated(&mut self) -> Vec<String> {
        self.finalize_inner(true)
    }

    fn finalize_inner(&mut self, truncated: bool) -> Vec<String> {
        if self.finalized {
            return Vec::new();
        }
        self.finalized = true;
        let mut events = Vec::new();

        if self.failed.is_some() {
            // 错误事件已在 process_chunk 发出；抑制正常收尾（不伪装成功）
            return events;
        }

        if truncated && !self.sent_message_start {
            // 零输出断流：error 事件（不伪装成成功）
            events.push(self.error_event("upstream stream ended before any output"));
            return events;
        }

        if !self.sent_message_start {
            // 空流但正常 [DONE]：补 message_start
            self.sent_message_start = true;
            events.push(self.message_start_event());
        }
        // M4：边界冲刷内联 think 缓冲（未闭合块整体算 thinking / 检测态残留算 text）
        let (think, text) = self.inline_think.flush();
        for r in think {
            events.extend(self.append_thinking_delta(&r));
        }
        for t in text {
            events.extend(self.append_text_delta(&t));
        }
        // 唯一输出是被丢弃的无名工具调用：不得伪装成空成功消息
        // （responses 管线同款 #4341 护栏；cc-switch 走 unknown_tool 合成，这里选择失败可见）
        // 先关块（计数 dropped_tools）再判定输出存在性。
        let close_events = self.close_open_blocks();
        // M16：块关闭后 started 复位，输出存在性改用 stopped 标志（曾打开过即算有输出）
        let has_output = self.thinking_stopped || self.text_stopped
            || self.tools.values().any(|t| t.started);
        if self.dropped_tools > 0 && !has_output {
            events.clear();
            events.push(self.error_event(
                "upstream returned tool_calls without function name and no other output",
            ));
            return events;
        }
        events.extend(close_events);
        events.extend(self.message_delta_events(truncated));
        events.push("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into());
        events
    }

    /// 记账用：usage 归一化为 store::Usage（含缓存桶，M8）
    pub fn billing_usage(&self) -> Option<crate::store::usage::Usage> {
        self.usage.as_ref().map(|u| crate::store::usage::Usage {
            prompt_tokens: Some(u.input()),
            completion_tokens: Some(u.output()),
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: Some(u.cached_tokens()),
            cache_write_tokens: Some(u.cache_write_tokens()),
        })
    }

    pub fn has_failed(&self) -> bool {
        self.failed.is_some()
    }

    // -- 事件构造 --

    fn message_start_event(&self) -> String {
        // 首块带 usage 时提前上报（部分上游在首 chunk 携带 input_tokens）
        let usage = self
            .usage
            .as_ref()
            .map(anthropic_usage)
            .unwrap_or_else(|| {
                json!({"input_tokens": 0, "output_tokens": 0,
                       "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0})
            });
        let message = json!({
            "id": format!("msg_{}", self.id),
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": [],
            "stop_reason": null,
            "stop_sequence": null,
            "usage": usage
        });
        sse_frame("message_start", &json!({"type": "message_start", "message": message}))
    }

    fn append_thinking_delta(&mut self, delta: &str) -> Vec<String> {
        let mut events = Vec::with_capacity(3);
        // M16：当前打开的是 text 块 → 先关再开新 thinking 块（单槽语义，
        // cc-switch streaming.rs:271-295 同款）；块序列严格按到达顺序
        let mut idx = match self.open_non_tool {
            Some(b) if b.kind == NonToolKind::Thinking => Some(b.index),
            _ => None,
        };
        if idx.is_none() {
            events.extend(self.close_open_non_tool());
            idx = Some(self.alloc_block_index());
            self.open_non_tool = Some(NonToolBlock {
                kind: NonToolKind::Thinking,
                index: idx.unwrap(),
            });
            self.thinking_block_index = idx.unwrap();
            self.thinking_started = true;
            self.non_tool_open_order.push(NonToolKind::Thinking);
            events.push(sse_frame(
                "content_block_start",
                &json!({
                    "type": "content_block_start", "index": idx,
                    "content_block": {"type": "thinking", "thinking": ""}
                }),
            ));
        }
        events.push(sse_frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta", "index": idx,
                "delta": {"type": "thinking_delta", "thinking": delta}
            }),
        ));
        events
    }

    fn append_text_delta(&mut self, delta: &str) -> Vec<String> {
        let mut events = Vec::with_capacity(3);
        let mut idx = match self.open_non_tool {
            Some(b) if b.kind == NonToolKind::Text => Some(b.index),
            _ => None,
        };
        if idx.is_none() {
            events.extend(self.close_open_non_tool());
            idx = Some(self.alloc_block_index());
            self.open_non_tool = Some(NonToolBlock {
                kind: NonToolKind::Text,
                index: idx.unwrap(),
            });
            self.text_block_index = idx.unwrap();
            self.text_started = true;
            self.non_tool_open_order.push(NonToolKind::Text);
            events.push(sse_frame(
                "content_block_start",
                &json!({
                    "type": "content_block_start", "index": idx,
                    "content_block": {"type": "text", "text": ""}
                }),
            ));
        }
        events.push(sse_frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta", "index": idx,
                "delta": {"type": "text_delta", "text": delta}
            }),
        ));
        events
    }

    /// M16：关闭当前打开的非工具块（若 text 打开则 stop text；若 thinking 打开则
    /// stop thinking）。块内 started 标志复位以便重开新块（多次交错的 text 产出
    /// 多个 text 块，属协议正确形态）
    fn close_open_non_tool(&mut self) -> Vec<String> {
        let mut events = Vec::with_capacity(1);
        if let Some(block) = self.open_non_tool.take() {
            events.push(block.stop_event());
            match block.kind {
                NonToolKind::Thinking => {
                    self.thinking_stopped = true;
                    self.thinking_started = false;
                }
                NonToolKind::Text => {
                    self.text_stopped = true;
                    self.text_started = false;
                }
            }
        }
        events
    }


    /// 工具增量：id+name 到齐才发 content_block_start；参数先缓冲（cc-switch 延迟启动）。
    /// M16：工具帧到达先关闭打开的非工具块（块序列单调）
    fn append_tool_delta(&mut self, tool_call: &ToolCallDelta) -> Vec<String> {
        let mut events = self.close_open_non_tool();
        // H4：缺 index 的帧按 cc-switch resolve_tool_key_without_index 语义解析
        //（新非空 id → 新 key；已知 id → 归回；无 id → 并入最后已知）
        let chat_index = match tool_call.index {
            Some(i) => i,
            None => {
                let key_for_id = tool_call
                    .id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .and_then(|id| {
                        self.tools.iter().find(|(_, b)| b.id == id).map(|(k, _)| *k)
                    });
                let last_key = self.last_tool_key;
                let max_key = self.tools.keys().copied().next_back();
                super::super::responses::dto::resolve_no_index_tool_key(
                    tool_call.id.as_deref(),
                    key_for_id,
                    last_key,
                    max_key,
                )
            }
        };
        self.last_tool_key = Some(chat_index);
        if !self.tools.contains_key(&chat_index) {
            let anthropic_index = self.alloc_block_index();
            self.tools.insert(
                chat_index,
                ToolBlock {
                    anthropic_index,
                    ..Default::default()
                },
            );
        }
        let block = self.tools.get_mut(&chat_index).expect("tool block exists");
        if let Some(id) = tool_call.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            block.id = id.to_string();
        }
        if let Some(name) = tool_call
            .function
            .as_ref()
            .and_then(|f| f.name.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            block.name = name.to_string();
        }
        let args_delta = tool_call
            .function
            .as_ref()
            .and_then(|f| f.arguments.as_deref())
            .filter(|a| !a.is_empty())
            .map(str::to_string);
        // 注意：events 已在函数头用 close_open_non_tool 初始化（M16 单槽关闭
        // 事件排在最前）
        if !block.started && !block.id.is_empty() && !block.name.is_empty() {
            block.started = true;
            self.has_tool_use = true;
            events.push(sse_frame(
                "content_block_start",
                &json!({
                    "type": "content_block_start", "index": block.anthropic_index,
                    "content_block": {"type": "tool_use", "id": block.id,
                                      "name": block.name, "input": {}}
                }),
            ));
            if !block.pending_args.is_empty() {
                let flushed = std::mem::take(&mut block.pending_args);
                block.arguments.push_str(&flushed);
                events.push(input_json_delta_event(block.anthropic_index, &flushed));
            }
        }
        if let Some(args) = args_delta {
            if block.aborted {
                return events;
            }
            // M6：连续空白达到阈值（Copilot 无限换行 bug）→ 中止该块参数下发，
            // 防止垃圾增量无限流出直至 120s idle timeout
            for ch in args.chars() {
                if ch.is_whitespace() {
                    block.consecutive_whitespace += 1;
                } else {
                    block.consecutive_whitespace = 0;
                }
            }
            if block.consecutive_whitespace >= INFINITE_WHITESPACE_THRESHOLD {
                if !block.aborted {
                    tracing::warn!(
                        tool = %block.name,
                        "infinite whitespace in tool arguments detected; aborting tool block"
                    );
                }
                block.aborted = true;
                return events;
            }
            if block.started {
                block.arguments.push_str(&args);
                events.push(input_json_delta_event(block.anthropic_index, &args));
            } else {
                block.pending_args.push_str(&args);
            }
        }
        events
    }

    /// 关闭所有打开的块：非工具块（按真实打开顺序）→ 工具块按 chat index 序。
    /// M17：未凑齐 id+name 但见过载荷的工具块合成 late-start
    /// （`tool_call_{idx}` / `unknown_tool` 兜底，cc-switch streaming.rs:542-602
    /// 同款）——调用对客户端始终可见；仅完全零载荷的帧才丢弃并计数。
    fn close_open_blocks(&mut self) -> Vec<String> {
        let mut events = self.close_open_non_tool();
        let indexes: Vec<i64> = self.tools.keys().copied().collect();
        for chat_index in indexes {
            let block = self.tools.get_mut(&chat_index).expect("tool exists");
            if block.started && !block.stopped {
                block.stopped = true;
                if block.id.is_empty() {
                    block.id = format!("tool_call_{chat_index}");
                }
                events.push(block_stop_event(block.anthropic_index));
            } else if !block.started {
                let has_payload = !block.id.is_empty()
                    || !block.name.is_empty()
                    || !block.pending_args.is_empty()
                    || !block.arguments.is_empty();
                if has_payload {
                    // M17：合成 late-start（id/name 缺省兜底，参数冲刷后再关块）
                    if block.id.is_empty() {
                        block.id = format!("tool_call_{chat_index}");
                    }
                    if block.name.is_empty() {
                        block.name = "unknown_tool".into();
                    }
                    block.started = true;
                    self.has_tool_use = true;
                    events.push(sse_frame(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start", "index": block.anthropic_index,
                            "content_block": {"type": "tool_use", "id": block.id,
                                              "name": block.name, "input": {}}
                        }),
                    ));
                    let flushed = std::mem::take(&mut block.pending_args);
                    if !flushed.is_empty() {
                        block.arguments.push_str(&flushed);
                        events.push(input_json_delta_event(block.anthropic_index, &flushed));
                    }
                    block.stopped = true;
                    events.push(block_stop_event(block.anthropic_index));
                } else {
                    self.dropped_tools += 1;
                    tracing::warn!(chat_index, "dropping streamed tool_call without function name");
                }
            }
        }
        events
    }

    /// message_delta（stop_reason + usage）；仅一次。断流时 stop_reason 缺省 max_tokens。
    fn message_delta_events(&mut self, truncated: bool) -> Vec<String> {
        if self.has_emitted_message_delta {
            return Vec::new();
        }
        self.has_emitted_message_delta = true;
        let stop_reason = self
            .pending_stop_reason
            .clone()
            .unwrap_or_else(|| {
                if truncated {
                    "max_tokens".into()
                } else if self.has_tool_use {
                    "tool_use".into()
                } else {
                    "end_turn".into()
                }
            });
        // P0-5：finish 帧时刻算出的 end_turn 早于 late-start 工具块（无名工具在
        // close_open_blocks 才置 has_tool_use）→ 消息含 tool_use 块却报 end_turn，
        // Claude Code 以 stop_reason 驱动工具循环会静默跳过执行。收尾时以最终
        // 块状态校正（cc-switch streaming.rs:537-545 无条件映射语义）
        let stop_reason = if stop_reason == "end_turn" && self.has_tool_use {
            String::from("tool_use")
        } else {
            stop_reason
        };
        let usage = self
            .usage
            .as_ref()
            .map(anthropic_usage)
            .unwrap_or_else(|| {
                json!({"input_tokens": 0, "output_tokens": 0,
                       "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0})
            });
        vec![sse_frame(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": usage
            }),
        )]
    }

    fn error_event(&self, message: &str) -> String {
        sse_frame(
            "error",
            &json!({
                "type": "error",
                "error": {"type": "stream_error", "message": message}
            }),
        )
    }

    fn alloc_block_index(&mut self) -> i64 {
        let index = self.next_block_index;
        self.next_block_index += 1;
        index
    }

}

fn input_json_delta_event(index: i64, partial_json: &str) -> String {
    sse_frame(
        "content_block_delta",
        &json!({
            "type": "content_block_delta", "index": index,
            "delta": {"type": "input_json_delta", "partial_json": partial_json}
        }),
    )
}

fn block_stop_event(index: i64) -> String {
    sse_frame(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": index}),
    )
}

fn sse_frame(event: &str, data: &Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}
// delta.content → 字符串提取已收敛至 responses::dto::delta_content_text
// （宽松：兼容 string 与 content-parts 数组两种上游形态）

// ---------------------------------------------------------------------------
// 流转换管线（代理接入点）
// ---------------------------------------------------------------------------

/// 客户端中断/流被丢弃时的兜底记账（与 tail 通过 recorded 标志互斥，只记一次）
struct BillingOnDrop {
    st: AppState,
    meta: UsageMeta,
    recorded: Arc<AtomicBool>,
    state: Arc<Mutex<ChatToAnthropicState>>,
    status: u16,
    latency: i64,
}

impl Drop for BillingOnDrop {
    fn drop(&mut self) {
        if !self.recorded.swap(true, Ordering::SeqCst) {
            let usage = self.state.lock().billing_usage();
            let st = self.st.clone();
            let meta = self.meta.clone();
            let status = self.status;
            let latency = self.latency;
            tokio::spawn(async move {
                tracing::warn!(request_id = %meta.request_id, "stream dropped before completion; billing best-effort");
                crate::service::usage::record(&st, &meta, usage.as_ref(), status, latency).await;
            });
        }
    }
}

/// 带兜底记账的转换流
pub struct BillingStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send>>,
    _billing: BillingOnDrop,
}

impl Stream for BillingStream {
    type Item = Result<Bytes, Box<dyn Error + Send + Sync>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().inner.as_mut().poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

/// 上游 Chat SSE → 客户端 Anthropic SSE：
/// - 逐帧解析 → `ChatToAnthropicState` 转换 → `event: {type}\ndata: {json}\n\n`
/// - 遇 [DONE]/错误帧立即收尾并记账；流异常中断由尾帧兜底（断流语义见 finalize_truncated）
pub fn wrap_chat_stream_to_anthropic(
    st: AppState,
    meta: UsageMeta,
    upstream: impl Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send + 'static,
    latency: i64,
    record_status: u16,
    message_id: String,
    model: String,
) -> BillingStream {
    let recorded = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let state: Arc<Mutex<ChatToAnthropicState>> =
        Arc::new(Mutex::new(ChatToAnthropicState::new(message_id, model)));
    let parser: Arc<Mutex<ChatSseParser>> = Arc::new(Mutex::new(ChatSseParser::new()));

    let main = upstream.then({
        let st = st.clone();
        let meta = meta.clone();
        let recorded = recorded.clone();
        let finished = finished.clone();
        let state = state.clone();
        let parser = parser.clone();
        move |chunk| {
            let st = st.clone();
            let meta = meta.clone();
            let recorded = recorded.clone();
            let finished = finished.clone();
            let state = state.clone();
            let parser = parser.clone();
            async move {
                // H1：上游传输错误/超时 → `event: error` 结构化事件（而非把 Err
                // 透传进 body 让 hyper 掐断客户端连接；cc-switch :627-642 同款）
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        if !finished.swap(true, Ordering::SeqCst) {
                            let (frames, usage) = {
                                let mut state = state.lock();
                                let mut frames = Vec::new();
                                for frame in &state.transport_error(&e.to_string()) {
                                    frames.extend_from_slice(frame.as_bytes());
                                }
                                (frames, state.billing_usage())
                            };
                            if !recorded.swap(true, Ordering::SeqCst) {
                                crate::service::usage::record(
                                    &st, &meta, usage.as_ref(), record_status, latency,
                                )
                                .await;
                            }
                            return Ok::<Bytes, Box<dyn Error + Send + Sync>>(Bytes::from(
                                frames,
                            ));
                        }
                        return Ok::<Bytes, Box<dyn Error + Send + Sync>>(Bytes::new());
                    }
                };
                let mut out: Vec<u8> = Vec::new();
                let (terminal, failed) = {
                    let mut parser = parser.lock();
                    let mut payloads: Vec<SseEvent> = Vec::new();
                    let t = parser.push(&chunk, &mut payloads);
                    let mut failed = false;
                    for frame in payloads {
                        let Some(mut cc) = ChatStreamChunk::from_json_str(&frame.data) else {
                            // JSON 语法非法才丢帧（类型怪癖由宽松解析兜住）；留痕不静默
                            tracing::warn!(
                                request_id = %meta.request_id,
                                payload_len = frame.data.len(),
                                "dropping malformed upstream SSE chunk"
                            );
                            continue;
                        };
                        // M3：`event: error` 名帧（data 无 error 字段）→ 失败终态
                        if cc.error.is_none() && frame.event.as_deref() == Some("error") {
                            cc.error = Some(json!({"message": frame.data}));
                        }
                        let mut state = state.lock();
                        for frame_str in state.process_chunk(&cc) {
                            out.extend_from_slice(frame_str.as_bytes());
                        }
                        failed |= state.has_failed();
                    }
                    (t, failed)
                };
                if (terminal || failed) && !finished.swap(true, Ordering::SeqCst) {
                    let (final_frames, usage) = {
                        let mut state = state.lock();
                        let mut frames = Vec::new();
                        for frame in &state.finalize() {
                            frames.extend_from_slice(frame.as_bytes());
                        }
                        (frames, state.billing_usage())
                    };
                    out.extend_from_slice(&final_frames);
                    if !recorded.swap(true, Ordering::SeqCst) {
                        crate::service::usage::record(
                            &st, &meta, usage.as_ref(), record_status, latency,
                        )
                        .await;
                    }
                }
                Ok::<Bytes, Box<dyn Error + Send + Sync>>(Bytes::from(out))
            }
        }
    });

    let tail = once({
        let st = st.clone();
        let meta = meta.clone();
        let recorded = recorded.clone();
        let finished = finished.clone();
        let state = state.clone();
        async move {
            if !finished.swap(true, Ordering::SeqCst) {
                let (final_frames, usage) = {
                    let mut state = state.lock();
                    let mut frames = Vec::new();
                    for frame in &state.finalize_truncated() {
                        frames.extend_from_slice(frame.as_bytes());
                    }
                    (frames, state.billing_usage())
                };
                if !recorded.swap(true, Ordering::SeqCst) {
                    crate::service::usage::record(
                        &st, &meta, usage.as_ref(), record_status, latency,
                    )
                    .await;
                }
                Ok::<Bytes, Box<dyn Error + Send + Sync>>(Bytes::from(final_frames))
            } else {
                Ok::<Bytes, Box<dyn Error + Send + Sync>>(Bytes::new())
            }
        }
    });

    BillingStream {
        inner: Box::pin(main.chain(tail)),
        _billing: BillingOnDrop {
            st,
            meta,
            recorded,
            state,
            status: record_status,
            latency,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::responses::dto::{ChatStreamChoice, ChatStreamDelta, FunctionDelta};

    fn chunk(
        content: Option<&str>,
        reasoning: Option<&str>,
        finish: Option<&str>,
        usage: Option<DtoUsage>,
        tool_calls: Vec<ToolCallDelta>,
    ) -> ChatStreamChunk {
        ChatStreamChunk {
            id: "abc".into(),
            model: "deepseek-chat".into(),
            created: Some(1700000000),
            choices: vec![ChatStreamChoice {
                index: 0,
                delta: ChatStreamDelta {
                    role: Some("assistant".into()),
                    content: content.map(|c| Value::String(c.into())),
                    reasoning_content: reasoning.map(str::to_string),
                    reasoning: None,
                    refusal: None,
                    tool_calls,
                },
                finish_reason: finish.map(str::to_string),
            }],
            usage,
            error: None,
        }
    }

    fn tool_delta(index: i64, id: Option<&str>, name: Option<&str>, args: Option<&str>) -> ToolCallDelta {
        ToolCallDelta {
            index: Some(index),
            id: id.map(str::to_string),
            r#type: Some("function".into()),
            function: Some(FunctionDelta {
                name: name.map(str::to_string),
                arguments: args.map(str::to_string),
            }),
        }
    }

    /// P0-5：finish 帧早于工具帧（无名 late-start）时，message_delta 的
    /// stop_reason 必须以最终块状态校正为 tool_use（此前早算 end_turn，
    /// Claude Code 会跳过工具执行）
    #[test]
    fn late_start_tool_overrides_end_turn_stop_reason() {
        let mut s = ChatToAnthropicState::new("resp_1".into(), "m".into());
        // finish 帧先行（此时尚无工具块 → 早算 end_turn）
        let mut frames = s.process_chunk(&chunk(None, None, Some("tool_calls"), None, vec![]));
        // 迟到无名参数帧 → late-start unknown_tool（close_open_blocks 才置 has_tool_use）
        frames.extend(s.process_chunk(&chunk(
            None,
            None,
            None,
            None,
            vec![tool_delta(0, Some("call_9"), None, Some("{\"a\":1}"))],
        )));
        frames.extend(s.finalize());
        let delta = frames
            .iter()
            .find(|f| f.contains("message_delta"))
            .expect("message_delta emitted");
        assert!(delta.contains("\"stop_reason\":\"tool_use\""), "{delta}");
    }

    #[test]
    fn full_event_sequence() {
        let mut s = ChatToAnthropicState::new("resp_1".into(), "m".into());
        let mut frames = Vec::new();
        frames.extend(s.process_chunk(&chunk(None, Some("thinking..."), None, None, vec![])));
        frames.extend(s.process_chunk(&chunk(Some("Hello"), None, None, None, vec![])));
        frames.extend(s.process_chunk(&chunk(
            None, None, None,
            Some(DtoUsage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                ..Default::default()
            }),
            vec![tool_delta(0, Some("call_1"), Some("get_weather"), Some("{\"city\""))],
        )));
        frames.extend(s.process_chunk(&chunk(
            None, None, Some("tool_calls"), None,
            vec![tool_delta(0, None, None, Some(":\"Paris\"}"))],
        )));
        frames.extend(s.finalize());

        let joined: String = frames.concat();
        // 生命周期顺序
        let seq: Vec<&str> = frames
            .iter()
            .filter_map(|f| f.strip_prefix("event: ").and_then(|r| r.split('\n').next()))
            .collect();
        assert_eq!(
            seq,
            vec![
                "message_start",
                "content_block_start",   // thinking（index 0）
                "content_block_delta",   // thinking_delta
                "content_block_stop",    // M16：text 到达先关 thinking
                "content_block_start",   // text（index 1）
                "content_block_delta",   // text_delta
                "content_block_stop",    // M16：工具帧到达先关 text
                "content_block_start",   // tool_use（index 2，id+name 到齐）
                "content_block_delta",   // input_json_delta {"city"
                "content_block_delta",   // input_json_delta :"Paris"}
                "content_block_stop",    // tool
                "message_delta",
                "message_stop",
            ]
        );
        assert!(joined.contains("\"partial_json\":\"{\\\"city\\\""));
    }


    #[test]
    fn content_parts_array_delta_emits_text_delta() {
        // Bug 回归：部分上游流式 delta.content 为 parts 数组，旧实现仅取 string
        // 形态 → 文本全丢 → message_stop 空内容（Claude Code 空回复）
        let mut s = ChatToAnthropicState::new("m1".into(), "m".into());
        let mut c = chunk(None, None, None, None, vec![]);
        c.choices[0].delta.content =
            Some(Value::Array(vec![json!({"type": "text", "text": "Hello parts"})]));
        let frames = s.process_chunk(&c);
        let joined: String = frames.concat();
        assert!(joined.contains("content_block_delta"));
        assert!(joined.contains("\"text\":\"Hello parts\""));
        assert!(joined.contains("\"type\":\"text_delta\""));
    }

    #[test]
    fn duplicate_finish_reason_deduped() {
        let mut s = ChatToAnthropicState::new("r".into(), "m".into());
        s.process_chunk(&chunk(Some("hi"), None, Some("stop"), None, vec![]));
        s.process_chunk(&chunk(None, None, Some("stop"), None, vec![])); // 重复 finish
        let frames = s.finalize();
        let deltas = frames
            .iter()
            .filter(|f| f.starts_with("event: message_delta"))
            .count();
        assert_eq!(deltas, 1, "message_delta 只发一次");
    }

    #[test]
    fn tool_args_buffered_until_identity_known() {
        let mut s = ChatToAnthropicState::new("r".into(), "m".into());
        // 参数先到（无 id/name）→ 缓冲
        let frames1 = s.process_chunk(&chunk(None, None, None, None,
            vec![tool_delta(0, None, None, Some("{\"a\""))]));
        assert!(frames1.iter().all(|f| !f.contains("content_block_start")));
        // id+name 到齐 → start + 冲刷缓冲 + 新增量
        let frames2 = s.process_chunk(&chunk(None, None, None, None,
            vec![tool_delta(0, Some("call_9"), Some("fn"), Some(":1}"))]));
        let joined: String = frames2.concat();
        assert!(joined.contains("\"type\":\"tool_use\""));
        assert!(joined.contains("\"partial_json\":\"{\\\"a\\\""));
        assert!(joined.contains("\"partial_json\":\":1}\""));
    }

    #[test]
    fn error_frame_emits_error_event_and_suppresses_close() {
        let mut s = ChatToAnthropicState::new("r".into(), "m".into());
        s.process_chunk(&chunk(Some("partial"), None, None, None, vec![]));
        let err_chunk = ChatStreamChunk {
            id: "r".into(),
            model: "m".into(),
            created: None,
            choices: vec![],
            usage: None,
            error: Some(json!({"message": "upstream exploded"})),
        };
        let frames = s.process_chunk(&err_chunk);
        let joined: String = frames.concat();
        assert!(joined.contains("event: error"));
        assert!(joined.contains("stream_error"));
        // 收尾：抑制 message_delta / message_stop
        let tail = s.finalize();
        assert!(tail.is_empty());
    }

    #[test]
    fn truncated_stream_with_output_gets_graceful_close() {
        let mut s = ChatToAnthropicState::new("r".into(), "m".into());
        s.process_chunk(&chunk(Some("partial answer"), None, None, None, vec![]));
        let frames = s.finalize_truncated();
        let joined: String = frames.concat();
        assert!(joined.contains("event: message_delta"));
        // 无 finish_reason 的断流 → max_tokens
        assert!(joined.contains("\"stop_reason\":\"max_tokens\""));
        assert!(joined.contains("event: message_stop"));
    }

    /// H4：缺 index 的并行工具帧按 id 区分，不坍缩、不串参
    #[test]
    fn missing_index_with_distinct_ids_keeps_tools_separate() {
        let no_index = |id: Option<&str>, name: Option<&str>, args: Option<&str>| ToolCallDelta {
            index: None,
            id: id.map(str::to_string),
            r#type: Some("function".into()),
            function: Some(FunctionDelta {
                name: name.map(str::to_string),
                arguments: args.map(str::to_string),
            }),
        };
        let mut s = ChatToAnthropicState::new("r".into(), "m".into());
        let frames1 = s.process_chunk(&chunk(None, None, None, None, vec![no_index(Some("call_a"), Some("read_file"), Some("{\"path\":\"a\"}"))]));
        let frames2 = s.process_chunk(&chunk(None, None, Some("tool_calls"), None, vec![no_index(Some("call_b"), Some("exec_command"), Some("{\"cmd\":\"ls\"}"))]));
        let joined: String = frames1.into_iter().chain(frames2).collect();
        assert!(joined.contains("\"id\":\"call_a\""));
        assert!(joined.contains("\"name\":\"read_file\""));
        assert!(joined.contains("\"id\":\"call_b\""));
        assert!(joined.contains("\"name\":\"exec_command\""));
        assert!(joined.contains("\"partial_json\":\"{\\\"cmd\\\":\\\"ls\\\"}\""));
        assert!(!joined.contains("\"name\":\"read_file\"},\"input\""), "无串参");
        // 两个块各自完整，参数不互相覆盖
        let starts = joined.matches("content_block_start").count() / 2;
        assert_eq!(starts, 2, "两个独立 tool_use 块");
    }
    /// H4：无 index 无 id 的续帧并入最后已知调用
    #[test]
    fn missing_index_continuation_merges_into_last_tool() {
        let no_index = |id: Option<&str>, name: Option<&str>, args: Option<&str>| ToolCallDelta {
            index: None,
            id: id.map(str::to_string),
            r#type: Some("function".into()),
            function: Some(FunctionDelta {
                name: name.map(str::to_string),
                arguments: args.map(str::to_string),
            }),
        };
        let mut s = ChatToAnthropicState::new("r".into(), "m".into());
        let frames1 = s.process_chunk(&chunk(None, None, None, None, vec![no_index(Some("call_1"), Some("f"), Some("{\"a\""))]));
        let frames2 = s.process_chunk(&chunk(None, None, None, None, vec![no_index(None, None, Some(":1}"))]));
        let joined: String = frames2.concat();
        assert!(joined.contains("\"partial_json\":\":1}\""), "续帧并入 call_1");
        assert_eq!(joined.matches("content_block_start").count(), 0, "续帧不新增块");
        assert_eq!(frames1.concat().matches("content_block_start").count() / 2, 1, "首帧启动一个块");
    }

    /// M4：content 里流首 <think> 块分离为 thinking
    #[test]
    fn inline_think_separated_into_thinking_block() {
        let mut s = ChatToAnthropicState::new("r".into(), "m".into());
        let frames = s.process_chunk(&chunk(Some("<think>hmm</think>Answer"), None, None, None, vec![]));
        let joined: String = frames.concat();
        assert!(joined.contains("\"type\":\"thinking_delta\""));
        assert!(joined.contains("\"thinking\":\"hmm\""));
        assert!(joined.contains("\"type\":\"text_delta\""));
        assert!(joined.contains("\"text\":\"Answer\""));
        // 块顺序：thinking(index 0) → text(index 1)
        let think_pos = joined.find("thinking_delta").unwrap();
        let text_pos = joined.find("text_delta").unwrap();
        assert!(think_pos < text_pos);
    }

    /// M6：连续空白达到阈值中止该工具块参数下发
    #[test]
    fn infinite_whitespace_aborts_tool_block() {
        let mut s = ChatToAnthropicState::new("r".into(), "m".into());
        s.process_chunk(&chunk(None, None, None, None, vec![tool_delta(0, Some("call_1"), Some("f"), Some("{\"x\""))]));
        let garbage = " ".repeat(600);
        let frames = s.process_chunk(&chunk(None, None, None, None, vec![tool_delta(0, None, None, Some(&garbage))]));
        let joined: String = frames.concat();
        assert!(!joined.contains(&garbage[..500]), "超阈值垃圾增量不得下发");
        // 后续正常增量也被抑制（块已中止）
        let frames = s.process_chunk(&chunk(None, None, None, None, vec![tool_delta(0, None, None, Some(":1}"))]));
        assert!(frames.concat().is_empty());
    }

    #[test]
    fn truncated_zero_output_emits_error() {
        let mut s = ChatToAnthropicState::new("r".into(), "m".into());
        let frames = s.finalize_truncated();
        let joined: String = frames.concat();
        assert!(joined.contains("event: error"));
        assert!(!joined.contains("message_stop"));
    }

    #[test]
    fn usage_three_buckets_in_message_delta() {
        let mut s = ChatToAnthropicState::new("r".into(), "m".into());
        s.process_chunk(&chunk(Some("x"), None, Some("stop"), Some(DtoUsage {
            prompt_tokens: Some(200),
            completion_tokens: Some(9),
            prompt_tokens_details: Some(json!({"cached_tokens": 150})),
            ..Default::default()
        }), vec![]));
        let frames = s.finalize();
        let joined: String = frames.concat();
        // 200 − 150 = 50 fresh input
        assert!(joined.contains("\"input_tokens\":50"));
        assert!(joined.contains("\"cache_read_input_tokens\":150"));
        assert!(joined.contains("\"output_tokens\":9"));
    }

    #[test]
    fn nameless_tool_late_starts_as_unknown_tool() {
        let mut s = ChatToAnthropicState::new("r".into(), "m".into());
        s.process_chunk(&chunk(None, None, None, None,
            vec![tool_delta(0, Some("call_1"), None, Some("{}"))]));
        let frames = s.finalize();
        let joined: String = frames.concat();
        // M17：有载荷的无名工具合成 late-start（unknown_tool 兜底），调用对客户端
        // 始终可见；零载荷才丢弃（cc-switch streaming.rs:542-602 同款）
        assert!(joined.contains("\"tool_use\""), "无名工具合成 tool_use 块");
        assert!(joined.contains("\"name\":\"unknown_tool\""), "name 兜底 unknown_tool");
        assert!(joined.contains("\"id\":\"call_1\""), "id 保留");
        assert!(joined.contains("event: message_stop"), "有工具输出 → 正常收尾");
        assert!(!joined.contains("event: error"), "不伪装成失败");
    }
}
