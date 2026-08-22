//! Chat Completions 流式响应 → Responses API 流式事件（SSE）。
//!
//! 移植自 new-api `relaykit/relayconvert/internal/oai_chat/to_oai_responses_stream_resp.go`。
//! `ChatToResponsesStreamState` 是增量状态机：逐 chunk 消费，产出
//! `response.created / output_item.added / output_text.delta / reasoning_summary_text.delta /
//! function_call_arguments.delta / *.done / output_item.done / response.completed(或 incomplete)` 事件。
//! 工具调用按 Chat `index` 关联；`finish_reason` 到达时先补发 done 事件，流结束（[DONE]）时发终态事件。

use std::collections::HashMap;
use std::error::Error;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::stream::{once, Stream, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::state::AppState;
use crate::store::usage::UsageMeta;

use super::convert_resp::finish_reason_to_status;
use super::dto::{
    delta_content_text, empty_annotations, output_item, summary_part, sse_frame, ChatStreamChunk,
    IncompleteDetailsOut, ResponsesOutputContentOut, ResponsesOutputOut, ResponsesResponseOut,
    ResponsesStreamEventOut, ToolCallDelta, Usage as DtoUsage,
};

const EVENT_CREATED: &str = "response.created";
const EVENT_IN_PROGRESS: &str = "response.in_progress";
const EVENT_COMPLETED: &str = "response.completed";
const EVENT_INCOMPLETE: &str = "response.incomplete";
const EVENT_FAILED: &str = "response.failed";
const EVENT_OUTPUT_ITEM_ADDED: &str = "response.output_item.added";
const EVENT_OUTPUT_ITEM_DONE: &str = "response.output_item.done";
const EVENT_OUTPUT_TEXT_DONE: &str = "response.output_text.done";
const EVENT_OUTPUT_TEXT_DELTA: &str = "response.output_text.delta";
const EVENT_CONTENT_PART_ADDED: &str = "response.content_part.added";
const EVENT_CONTENT_PART_DONE: &str = "response.content_part.done";
const EVENT_REASONING_SUMMARY_DELTA: &str = "response.reasoning_summary_text.delta";
const EVENT_REASONING_SUMMARY_DONE: &str = "response.reasoning_summary_text.done";
const EVENT_REASONING_SUMMARY_PART_ADDED: &str = "response.reasoning_summary_part.added";
const EVENT_REASONING_SUMMARY_PART_DONE: &str = "response.reasoning_summary_part.done";
const EVENT_FUNCTION_ARGS_DELTA: &str = "response.function_call_arguments.delta";
const EVENT_FUNCTION_ARGS_DONE: &str = "response.function_call_arguments.done";
const EVENT_CUSTOM_INPUT_DELTA: &str = "response.custom_tool_call_input.delta";
const EVENT_CUSTOM_INPUT_DONE: &str = "response.custom_tool_call_input.done";
#[derive(Debug, Clone, Copy, PartialEq)]
enum OutputRef {
    Message,
    Reasoning,
    Tool(i64),
}
/// done 事件面分流：普通 function / M6 custom / 四-4 桥接 / P0-4 tool_search
#[derive(Debug, Clone, Copy, PartialEq)]
enum ToolDoneKind {
    Function,
    Custom,
    Bridged,
    ToolSearch,
}


/// 单个 Chat 工具调用（按 Chat choice.delta.tool_calls 的 index 关联）
#[derive(Debug, Default)]
struct ChatTool {
    output_index: i64,
    id: String,
    name: String,
    arguments: String,
    /// L6：归属本工具调用的思考文本（reasoning 帧累积；item.added/done 附挂）
    reasoning_content: String,
    /// 四-4：本调用桥接为 web_search_call item（请求声明了 hosted web_search）
    bridged: bool,
    /// M6：本调用是请求声明的 custom 工具降级而来 → 输出 custom_tool_call item
    ///（input 事件面替代 arguments 事件面）
    custom: bool,
    /// P0-4：本调用是 tool_search 代理调用 → 输出 tool_search_call item
    tool_search: bool,
    /// P0-4：namespace 工具还原（拍平 chat 名 → 原始短名 + namespace）
    orig_name: Option<String>,
    namespace: Option<String>,
    /// output_item.added 已发出（id+name 到齐后才发出，防迟到续帧乱序）
    started: bool,
    /// L5：added 已按 chat index 顺序释放（乱序身份帧时按最小未释放序出，
    /// cc-switch flush_ready_tool_calls 同款）
    released: bool,
    /// added 发出前缓冲的参数增量
    pending_args: String,
    done: bool,
}



/// Chat 流 → Responses 流 状态机
pub struct ChatToResponsesStreamState {
    pub id: String,
    pub model: String,
    pub created: i64,
    /// 上游 Chat usage（累积自 chunk；终态事件与记账共用）
    pub usage: Option<DtoUsage>,

    status: String,
    incomplete_details: Option<IncompleteDetailsOut>,
    /// 上游流内错误帧（chunk.error）→ 终态 response.failed
    failed_error: Option<Value>,
    /// 是否已收到 finish_reason（H2：finish 已到但流无 [DONE] 时不得误标 incomplete）
    finish_reason_seen: bool,
    dropped_tools: usize,
    sent_created: bool,
    text_output_index: i64,
    text_started: bool,
    text_done: bool,
    reasoning_output_index: i64,
    reasoning_started: bool,
    reasoning_done: bool,
    finalized: bool,
    next_output_index: i64,
    tools_by_index: HashMap<i64, ChatTool>,
    /// 最后一个收到增量的工具 key（无 index 帧的"并入最后已知"用，H4）
    last_tool_key: Option<i64>,
    output_order: Vec<OutputRef>,
    text: String,
    reasoning: String,
    /// 内联 <think> 分离器（M4：上游把思考混在 content 里时分离为 reasoning）
    inline_think: crate::service::inline_think::InlineThinkState,
    /// P0-4：请求工具上下文（custom / namespace / tool_search / web_search 桥接
    /// 的响应侧还原依据；替代原 bridge_web_search + custom_tools 两个独立通道）
    tool_ctx: super::tool_ctx::ToolContext,
}

impl ChatToResponsesStreamState {
    pub fn new(id: String, model: String, tool_ctx: super::tool_ctx::ToolContext) -> Self {
        Self {
            id,
            model,
            created: now_secs(),
            usage: None,
            status: "completed".into(),
            incomplete_details: None,
            failed_error: None,
            finish_reason_seen: false,
            dropped_tools: 0,
            sent_created: false,
            text_output_index: -1,
            text_started: false,
            text_done: false,
            reasoning_output_index: -1,
            reasoning_started: false,
            reasoning_done: false,
            finalized: false,
            next_output_index: 0,
            tools_by_index: HashMap::new(),
            last_tool_key: None,
            output_order: Vec::new(),
            text: String::new(),
            reasoning: String::new(),
            inline_think: crate::service::inline_think::InlineThinkState::new(),
            tool_ctx,
        }
    }


    /// 消费一个 Chat chunk，产出 0..n 个 Responses 事件。
    /// 上游错误帧（chunk.error）置 failed 状态并停止转换（终态由 finalize 发出）。
    pub fn process_chunk(&mut self, chunk: &ChatStreamChunk) -> Vec<ResponsesStreamEventOut> {
        if self.failed_error.is_some() {
            return Vec::new();
        }
        if let Some(err) = &chunk.error {
            if self.failed_error.is_none() {
                self.failed_error = Some(extract_stream_error(err));
            }
            return Vec::new();
        }
        if self.id.is_empty() && !chunk.id.is_empty() {
            self.id = chunk.id.clone();
        }
        if self.model.is_empty() && !chunk.model.is_empty() {
            self.model = chunk.model.clone();
        }
        if self.created == 0 {
            if let Some(c) = chunk.created {
                self.created = c;
            }
        }
        if let Some(usage) = &chunk.usage {
            self.usage = Some(usage.clone());
        }

        let mut events = Vec::new();
        if !self.sent_created {
            self.sent_created = true;
            events.push(self.created_event());
            // cc-switch ensure_response_started :270-282 同款：created + in_progress 双事件
            events.push(self.in_progress_event());
        }
        for choice in &chunk.choices {
            if let Some(r) = choice
                .delta
                .reasoning_content
                .as_deref()
                .or(choice.delta.reasoning.as_deref())
                .filter(|r| !r.is_empty())
            {
                events.extend(self.append_reasoning_delta(r));
            }
            if let Some(content) = choice
                .delta
                .content
                .as_ref()
                .and_then(delta_content_text)
                .filter(|c| !c.is_empty())
            {
                // M4：流首 <think>…</think> 块分离为 reasoning，其余为正文
                let (think, text) = self.inline_think.feed(&content);
                for r in think {
                    events.extend(self.append_reasoning_delta(&r));
                }
                for t in text {
                    events.extend(self.append_text_delta(&t));
                }
            }
            // 四-3：流式 refusal → output_text（非流式已有 refusal→text 兜底；
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
                // L6：工具帧到达时关闭 reasoning item，把当前思考文本附到
                // function_call item 的 reasoning_content（cc-switch
                // streaming_codex_chat.rs:161-167 同款：思考归属工具调用保真度）
                let (close_events, reasoning_text) = self.close_reasoning_for_tool_call();
                events.extend(close_events);
                events.extend(self.append_tool_call_delta(tool_call, reasoning_text.as_deref()));
            }
            if let Some(fr) = choice
                .finish_reason
                .as_deref()
                .map(str::trim)
                .filter(|f| !f.is_empty())
            {
                self.apply_finish_reason(fr);
                events.extend(self.done_delta_events());
            }
        }
        events
    }

    /// 流正常结束（[DONE]）：补发终态事件；幂等
    pub fn finalize(&mut self) -> Vec<ResponsesStreamEventOut> {
        self.finalize_inner(false)
    }

    /// 流异常结束（上游断流，无 [DONE]）：
    /// - 已有实质输出 → 合成 incomplete/max_output_tokens（cc-switch 断流兜底）
    /// - 零输出 → response.failed/stream_truncated
    pub fn finalize_truncated(&mut self) -> Vec<ResponsesStreamEventOut> {
        self.finalize_inner(true)
    }

    fn finalize_inner(&mut self, truncated: bool) -> Vec<ResponsesStreamEventOut> {
        if self.finalized {
            return Vec::new();
        }
        self.finalized = true;
        // M4：边界冲刷内联 think 缓冲（未闭合块整体算 reasoning / 检测态残留算正文）
        let (think, text) = self.inline_think.flush();
        let mut events = Vec::new();
        for r in think {
            events.extend(self.append_reasoning_delta(&r));
        }
        for t in text {
            events.extend(self.append_text_delta(&t));
        }
        events.extend(self.done_delta_events());

        // 终态判定：错误帧 > 断流 > 丢弃护栏 > 常规
        let (event_type, error) = if let Some(err) = self.failed_error.clone() {
            self.status = "failed".into();
            (EVENT_FAILED, Some(err))
        } else if truncated && !self.has_any_output() {
            self.status = "failed".into();
            (
                EVENT_FAILED,
                Some(json!({
                    "code": "stream_truncated",
                    "message": "upstream stream ended before completion"
                })),
            )
        // H2：finish_reason 已到但流未以 [DONE] 收尾 → 尊重 finish_reason 推导的
        // status（完整回答不得误标 incomplete/max_output_tokens，cc-switch
        // streaming_codex_chat.rs:888-903 同款）；仅"无 finish_reason 断流"合成 incomplete
        } else if truncated && !self.finish_reason_seen {
            self.status = "incomplete".into();
            self.incomplete_details = Some(IncompleteDetailsOut {
                reason: "max_output_tokens".into(),
            });
            (EVENT_INCOMPLETE, None)
        } else if self.dropped_tools > 0 && !self.has_any_output() && self.status != "incomplete" {
            self.status = "failed".into();
            (
                EVENT_FAILED,
                Some(json!({
                    "code": "upstream_tool_call_dropped",
                    "message": "upstream returned tool_calls without function name and no other output"
                })),
            )
        } else {
            let ty = if self.status == "incomplete" {
                EVENT_INCOMPLETE
            } else {
                EVENT_COMPLETED
            };
            (ty, None)
        };

        let mut resp = self.final_response();
        if let Some(e) = &error {
            resp.error = Some(e.clone());
        }
        events.push(ResponsesStreamEventOut {
            r#type: event_type.into(),
            response: Some(resp),
            delta: None,
            item: None,
            output_index: None,
            content_index: None,
            summary_index: None,
            item_id: None,
            part: None,
            input: None,
            text: None,
            arguments: None,
        });
        events
    }

    /// 是否有任何实质输出（文本/推理/已启动的有效工具）
    fn has_any_output(&self) -> bool {
        self.text_started
            || self.reasoning_started
            || self
                .tools_by_index
                .values()
                .any(|t| t.started && !t.name.is_empty())
    }

    /// 记账用：状态内 usage 归一化为 store::Usage（双形态回退 + 缓存桶，M8）
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

    /// 上游错误帧已到达（wrap 层据此立即补发终态）
    pub fn has_failed(&self) -> bool {
        self.failed_error.is_some()
    }

    /// H1：上游传输错误（流 Err 项）→ 置 failed 并产出终态事件（response.failed）。
    /// 客户端收到结构化错误而非 TCP 截断（此前 Err 直通 hyper 掐断连接）。
    pub fn record_transport_error(&mut self, message: &str) -> Vec<ResponsesStreamEventOut> {
        if self.failed_error.is_none() {
            self.failed_error = Some(json!({
                "code": "stream_error",
                "message": message
            }));
        }
        self.finalize()
    }

    // -- 内部 --

    fn created_event(&self) -> ResponsesStreamEventOut {
        ResponsesStreamEventOut {
            r#type: EVENT_CREATED.into(),
            response: Some(ResponsesResponseOut {
                id: self.id.clone(),
                object: "response",
                created_at: self.created,
                status: "in_progress".into(),
                error: None,
                incomplete_details: None,
                instructions: None,
                max_output_tokens: None,
                model: self.model.clone(),
                output: Vec::new(),
                parallel_tool_calls: None,
                previous_response_id: None,
                reasoning: None,
                store: None,
                temperature: None,
                tool_choice: None,
                tools: None,
                top_p: None,
                truncation: None,
                usage: None,
                user: None,
                metadata: None,
            }),
            delta: None,
            item: None,
            output_index: None,
            content_index: None,
            summary_index: None,
            item_id: None,
            part: None,
            input: None,
            text: None,
            arguments: None,
        }
    }

    /// response.in_progress（created 后紧跟；cc-switch :280 同款）
    fn in_progress_event(&self) -> ResponsesStreamEventOut {
        let mut ev = self.created_event();
        ev.r#type = EVENT_IN_PROGRESS.into();
        ev
    }

    fn append_text_delta(&mut self, delta: &str) -> Vec<ResponsesStreamEventOut> {
        let mut events = Vec::with_capacity(3);
        if !self.text_started {
            self.text_started = true;
            self.text_output_index = self.next_index(OutputRef::Message);
            let mut item = output_item("message", self.message_id(), "in_progress", Vec::new());
            item.role = "assistant".into();
            events.push(ResponsesStreamEventOut {
                r#type: EVENT_OUTPUT_ITEM_ADDED.into(),
                response: None,
                delta: None,
                item: Some(item),
                output_index: Some(self.text_output_index),
                content_index: None,
                summary_index: None,
                item_id: None,
                part: None,
                input: None,
                text: None,
                arguments: None,
            });
            // content_part.added 先于首个 text delta（cc-switch / 真实 Responses 事件序）
            events.push(self.content_part_event(
                EVENT_CONTENT_PART_ADDED,
                super::dto::summary_part("output_text", ""),
                true,
            ));
        }
        self.text.push_str(delta);
        events.push(ResponsesStreamEventOut {
            r#type: EVENT_OUTPUT_TEXT_DELTA.into(),
            response: None,
            delta: Some(delta.to_string()),
            item: None,
            output_index: Some(self.text_output_index),
            content_index: Some(0),
            summary_index: None,
            item_id: Some(self.message_id()),
            part: None,
            input: None,
            text: None,
            arguments: None,
        });
        events
    }

    fn append_reasoning_delta(&mut self, delta: &str) -> Vec<ResponsesStreamEventOut> {
        let mut events = Vec::with_capacity(4);
        // L6：工具帧关闭过的 reasoning item，后续思考重开（新 added 事件，index 复用）
        let reopen = self.reasoning_done;
        if !self.reasoning_started || reopen {
            self.reasoning_started = true;
            self.reasoning_done = false;
            if !reopen {
                self.reasoning_output_index = self.next_index(OutputRef::Reasoning);
            }
            let mut item = output_item("reasoning", self.reasoning_id(), "in_progress", Vec::new());
            item.summary = Vec::new();
            events.push(ResponsesStreamEventOut {
                r#type: EVENT_OUTPUT_ITEM_ADDED.into(),
                response: None,
                delta: None,
                item: Some(item),
                output_index: Some(self.reasoning_output_index),
                content_index: None,
                summary_index: None,
                item_id: None,
                part: None,
                input: None,
                text: None,
                arguments: None,
            });
            // reasoning_summary_part.added 先于首个 reasoning delta
            events.push(ResponsesStreamEventOut {
                r#type: EVENT_REASONING_SUMMARY_PART_ADDED.into(),
                response: None,
                delta: None,
                item: None,
                output_index: Some(self.reasoning_output_index),
                content_index: None,
                summary_index: Some(0),
                item_id: Some(self.reasoning_id()),
                part: Some(summary_part("summary_text", "")),
                input: None,
                text: None,
                arguments: None,
            });
        }
        self.reasoning.push_str(delta);
        // L6：思考归属活跃工具（cc-switch append_reasoning_to_active_tools 同款）
        for tool in self.tools_by_index.values_mut() {
            if tool.started {
                tool.reasoning_content.push_str(delta);
            }
        }
        events.push(ResponsesStreamEventOut {
            r#type: EVENT_REASONING_SUMMARY_DELTA.into(),
            response: None,
            delta: Some(delta.to_string()),
            item: None,
            output_index: Some(self.reasoning_output_index),
            content_index: None,
            summary_index: Some(0),
            item_id: Some(self.reasoning_id()),
            part: None,
            input: None,
            text: None,
            arguments: None,
        });
        events
    }

    /// L6：工具帧到达时关闭 reasoning item（终态事件），返回当前思考全文
    /// 供 function_call item 附挂（cc-switch finalize_reasoning +
    /// current_reasoning_text，streaming_codex_chat.rs:161-167 同款）。
    /// 未开启或已关闭 → 空事件 + None（幂等）。
    fn close_reasoning_for_tool_call(
        &mut self,
    ) -> (Vec<ResponsesStreamEventOut>, Option<String>) {
        if !self.reasoning_started || self.reasoning_done {
            return (Vec::new(), None);
        }
        self.reasoning_done = true;
        let mut events = Vec::with_capacity(3);
        events.push(self.reasoning_done_event());
        events.push(self.reasoning_summary_part_done_event());
        events.push(self.output_item_done_event(
            self.reasoning_output_index,
            self.reasoning_output("completed"),
        ));
        let text = self.reasoning.trim();
        (
            events,
            (!text.is_empty()).then(|| text.to_string()),
        )
    }
    /// 工具调用增量：id+name 到齐前不发 output_item.added（cc-switch
    /// flush_ready_tool_calls 语义），参数增量先缓冲、启动后一次性冲刷 ——
    /// 防止迟到 identity 续帧造成事件乱序；最终仍无名的调用在 finalize 丢弃
    /// `reasoning`：本帧关闭的思考全文（L6），附挂到新启动工具的 item
    fn append_tool_call_delta(
        &mut self,
        tool_call: &ToolCallDelta,
        reasoning: Option<&str>,
    ) -> Vec<ResponsesStreamEventOut> {
        // H4：缺 index 的帧不再一律归 key 0（并行无 index 调用会互相覆盖 id/name、
        // arguments 串接成非法 JSON）。cc-switch resolve_tool_key_without_index 语义：
        // 新非空 id（与所有已知调用不同）→ 新 key；已知 id → 归回；无 id → 并入最后已知
        let chat_index = match tool_call.index {
            Some(i) => i,
            None => {
                let key_for_id = tool_call
                    .id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .and_then(|id| {
                        self.tools_by_index
                            .iter()
                            .find(|(_, t)| t.id == id)
                            .map(|(k, _)| *k)
                    });
                let last_key = self.last_tool_key;
                let max_key = self.tools_by_index.keys().copied().max();
                super::dto::resolve_no_index_tool_key(tool_call.id.as_deref(), key_for_id, last_key, max_key)
            }
        };
        self.last_tool_key = Some(chat_index);
        let mut events = Vec::with_capacity(3);

        if !self.tools_by_index.contains_key(&chat_index) {
            // L5：output_index 在释放时分配（乱序身份帧按 chat index 序出，
            // output 顺序与生成序一致；cc-switch flush_ready_tool_calls 同款）
            self.tools_by_index.insert(
                chat_index,
                ChatTool {
                    output_index: -1,
                    ..Default::default()
                },
            );
        }
        let tool = self.tools_by_index.get_mut(&chat_index).expect("tool exists");

        if let Some(id) = tool_call.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            tool.id = id.to_string();
        }
        if let Some(name) = tool_call.function.as_ref().and_then(|f| f.name.as_deref()) {
            let name = name.trim();
            if !name.is_empty() {
                tool.name = name.to_string();
                // 四-4：请求声明的 hosted web_search → 桥接为 web_search_call item
                tool.bridged = self.tool_ctx.bridges_web_search && tool.name == "web_search";
                // M6 / P0-4：按请求工具上下文还原 custom / namespace / tool_search
                if let Some(spec) = self.tool_ctx.lookup(&tool.name) {
                    match spec.kind {
                        super::tool_ctx::ToolKind::Custom => tool.custom = true,
                        super::tool_ctx::ToolKind::ToolSearch => tool.tool_search = true,
                        super::tool_ctx::ToolKind::Namespace => {
                            tool.orig_name = Some(spec.name.clone());
                            tool.namespace = spec.namespace.clone();
                        }
                        super::tool_ctx::ToolKind::Function => {}
                    }
                }
            }
        }
        let args_delta = tool_call
            .function
            .as_ref()
            .and_then(|f| f.arguments.as_deref())
            .filter(|a| !a.is_empty())
            .map(str::to_string);

        // L6：本帧关闭的思考全文附挂到新启动的工具（若推理帧已在活跃期累积过
        // 则已在 reasoning_content 中，此处只补工具启动前关闭的部分）
        if let Some(r) = reasoning.filter(|r| !r.is_empty()) {
            if tool.reasoning_content.is_empty() {
                tool.reasoning_content = r.to_string();
            }
        }
        // 启动条件满足：标记 started（added 事件由 release_ready_tools 按
        // chat index 序释放；迟到续帧仍安全——未释放前参数只缓冲）
        if !tool.started && !tool.id.is_empty() && !tool.name.is_empty() {
            tool.started = true;
        }

        if let Some(args) = args_delta {
            if tool.started {
                if tool.released {
                    // 已释放：参数一律累积（bridged/custom/tool_search 不发参数事件面）
                    tool.arguments.push_str(&args);
                    if !tool.bridged && !tool.custom && !tool.tool_search {
                        events.push(ResponsesStreamEventOut {
                            r#type: EVENT_FUNCTION_ARGS_DELTA.into(),
                            response: None,
                            delta: Some(args),
                            item: None,
                            output_index: Some(tool.output_index),
                            content_index: None,
                            summary_index: None,
                            // P0-1：item_id 与 output_item.added/done 的 item.id
                            // 使用同一前缀 id（fc_；此前用裸 tool.id，客户端按
                            // item_id↔item.id 关联增量时归属断裂）
                            item_id: Some(Self::tool_item_id(tool)),
                            part: None,
                            input: None,
                            text: None,
                            arguments: None,
                        });
                    }
                } else {
                    // started 未释放：只缓冲（释放时冲刷；不得同时入 arguments，
                    // 否则释放冲刷时参数重复）
                    tool.pending_args.push_str(&args);
                }
            } else {
                // 未启动：缓冲（启动时作为首个 arguments delta 冲刷）
                tool.pending_args.push_str(&args);
            }
        }
        events.extend(self.release_ready_tools(false));
        events
    }

    /// L5：把「最小未释放 chat index 且已 started」的工具按序释放
    /// （output_item.added + 冲刷缓冲参数）；乱序身份帧等待更低 index 到齐，
    /// cc-switch flush_ready_tool_calls / next_tool_index_to_add 同款语义。
    /// `force`：finish 收尾时强制按序释放所有 started 未释放工具（被无名低
    /// index 卡住的调用不得静默消失）
    fn release_ready_tools(&mut self, force: bool) -> Vec<ResponsesStreamEventOut> {
        let mut events = Vec::new();
        loop {
            let min_candidate = self
                .tools_by_index
                .iter()
                .filter(|(_, t)| !t.released && (!force || t.started))
                .map(|(k, _)| *k)
                .min();
            let Some(chat_index) = min_candidate else {
                break;
            };
            if !self
                .tools_by_index
                .get(&chat_index)
                .is_some_and(|t| t.started)
            {
                // 最小未释放未启动（force 时已被过滤，不会走到）→ 等待
                break;
            }
            let output_index = self.next_index(OutputRef::Tool(chat_index));
            let tool = self.tools_by_index.get_mut(&chat_index).expect("tool exists");
            tool.output_index = output_index;
            tool.released = true;
            events.push(ResponsesStreamEventOut {
                r#type: EVENT_OUTPUT_ITEM_ADDED.into(),
                response: None,
                delta: None,
                item: Some(Self::tool_output_item(tool, "in_progress")),
                output_index: Some(output_index),
                content_index: None,
                summary_index: None,
                item_id: None,
                part: None,
                input: None,
                text: None,
                arguments: None,
            });
            if !tool.pending_args.is_empty() {
                let flushed = std::mem::take(&mut tool.pending_args);
                tool.arguments.push_str(&flushed);
                // 四-4 桥接 / M6 custom / P0-4 tool_search 不发 function_call_arguments.delta
                if !tool.bridged && !tool.custom && !tool.tool_search {
                    events.push(ResponsesStreamEventOut {
                        r#type: EVENT_FUNCTION_ARGS_DELTA.into(),
                        response: None,
                        delta: Some(flushed),
                        item: None,
                        output_index: Some(output_index),
                        content_index: None,
                        summary_index: None,
                        // P0-1：与 added/done 的 item.id 同前缀（见 append_tool_call_delta）
                        item_id: Some(Self::tool_item_id(tool)),
                        part: None,
                        input: None,
                        text: None,
                        arguments: None,
                    });
                }
            }
        }
        events
    }


    /// finish_reason → status（length/content_filter → incomplete）
    fn apply_finish_reason(&mut self, finish_reason: &str) {
        self.finish_reason_seen = true;
        let (status, details) = finish_reason_to_status(finish_reason);
        if status == "incomplete" {
            self.status = status;
            self.incomplete_details = details;
        }
    }

    /// 补发各 output 的 done 事件（finish_reason 到达/终态时；按 *_done 标志幂等）
    fn done_delta_events(&mut self) -> Vec<ResponsesStreamEventOut> {
        let mut events = self.release_ready_tools(false);
        events.extend(self.release_ready_tools(true));
        let status = self.output_status();
        if self.text_started && !self.text_done {
            self.text_done = true;
            events.push(self.text_done_event());
            events.push(self.content_part_done_event());
            events.push(self.output_item_done_event(
                self.text_output_index,
                self.message_output(status),
            ));
        }
        if self.reasoning_started && !self.reasoning_done {
            self.reasoning_done = true;
            events.push(self.reasoning_done_event());
            events.push(self.reasoning_summary_part_done_event());
            events.push(self.output_item_done_event(
                self.reasoning_output_index,
                self.reasoning_output(status),
            ));
        }
        let mut indexes: Vec<i64> = self.tools_by_index.keys().copied().collect();
        indexes.sort_unstable();
        for chat_index in indexes {
            let (output_index, item, kind) = {
                let tool = self.tools_by_index.get_mut(&chat_index).expect("tool exists");
                if tool.done {
                    continue;
                }
                tool.done = true;
                // 缺函数名的调用无法执行：整只丢弃（#4341），不留空 name 的 item
                if tool.name.is_empty() {
                    self.dropped_tools += 1;
                    tracing::warn!(chat_index, "dropping streamed tool_call without function name");
                    continue;
                }
                // id 缺失时兜底（cc-switch late-start 兜底同款），保持 call_id 可寻址
                if tool.id.is_empty() {
                    tool.id = format!("{}_call_{}", self.id, chat_index);
                }
                (
                    tool.output_index,
                    Self::tool_output_item(tool, status),
                    if tool.bridged {
                        ToolDoneKind::Bridged
                    } else if tool.tool_search {
                        ToolDoneKind::ToolSearch
                    } else if tool.custom {
                        ToolDoneKind::Custom
                    } else {
                        ToolDoneKind::Function
                    },
                )
            };
            match kind {
                // 四-4：桥接工具无参数事件面（query 已并入 done item）
                ToolDoneKind::Bridged => {}
                // P0-4：tool_search 代理调用无参数事件面（arguments 已并入 done item）
                ToolDoneKind::ToolSearch => {}
                //（cc-switch finalize_tools :703-716 同款：input 从累积参数一次性还原）
                ToolDoneKind::Custom => {
                    let input = match &item {
                        v if !v.input.as_deref().unwrap_or("").is_empty() => {
                            v.input.clone().unwrap_or_default()
                        }
                        _ => String::new(),
                    };
                    // 事件面的 item_id 与 added 事件的 item.id 一致（ctc_ 前缀）
                    let item_id = item.id.clone();
                    if !input.is_empty() {
                        events.push(ResponsesStreamEventOut {
                            r#type: EVENT_CUSTOM_INPUT_DELTA.into(),
                            response: None,
                            delta: Some(input.clone()),
                            item: None,
                            output_index: Some(output_index),
                            content_index: None,
                            summary_index: None,
                            item_id: Some(item_id.clone()),
                            part: None,
                            input: None,
                            text: None,
                            arguments: None,
                        });
                    }
                    events.push(ResponsesStreamEventOut {
                        r#type: EVENT_CUSTOM_INPUT_DONE.into(),
                        response: None,
                        delta: None,
                        item: None,
                        output_index: Some(output_index),
                        content_index: None,
                        summary_index: None,
                        item_id: Some(item_id),
                        part: None,
                        input: Some(input),
                        text: None,
                        arguments: None,
                    });
                }
                ToolDoneKind::Function => {
                    // M1：arguments.done 带 canonical 全参（cc-switch :310-327 同款）
                    let canonical_args = {
                        let tool = self.tools_by_index.get(&chat_index);
                        tool.map(|t| {
                            crate::service::canonical::canonicalize_json_string_if_parseable(
                                &t.arguments,
                            )
                        })
                    };
                    // P0-1：done 事件 item_id 用与 item.id 相同的 fc_ 前缀 id
                    let done_item_id = {
                        let tool = self.tools_by_index.get(&chat_index);
                        tool.map(Self::tool_item_id).unwrap_or_else(|| item.id.clone())
                    };
                    events.push(ResponsesStreamEventOut {
                        r#type: EVENT_FUNCTION_ARGS_DONE.into(),
                        response: None,
                        delta: None,
                        item: None,
                        output_index: Some(output_index),
                        content_index: None,
                        summary_index: None,
                        item_id: Some(done_item_id),
                        part: None,
                        input: None,
                        text: None,
                        arguments: canonical_args,
                    });
                }
            }
            events.push(self.output_item_done_event(output_index, item));
        }
        events
    }

    fn text_done_event(&self) -> ResponsesStreamEventOut {
        ResponsesStreamEventOut {
            r#type: EVENT_OUTPUT_TEXT_DONE.into(),
            response: None,
            delta: None,
            item: None,
            output_index: Some(self.text_output_index),
            content_index: Some(0),
            summary_index: None,
            item_id: Some(self.message_id()),
            part: None,
            input: None,
            // M1：done 事件带聚合全文（cc-switch codex_responses_sse.rs:184-208 同款）
            text: Some(self.text.clone()),
            arguments: None,
        }
    }

    /// content_part.added / done（message 输出的 output_text part）
    fn content_part_event(
        &self,
        ty: &str,
        mut part: super::dto::SummaryPartOut,
        with_annotations: bool,
    ) -> ResponsesStreamEventOut {
        if with_annotations {
            part.annotations = Some(Vec::new());
        }
        ResponsesStreamEventOut {
            r#type: ty.into(),
            response: None,
            delta: None,
            item: None,
            output_index: Some(self.text_output_index),
            content_index: Some(0),
            summary_index: None,
            item_id: Some(self.message_id()),
            part: Some(part),
            input: None,
            text: None,
            arguments: None,
        }
    }

    fn content_part_done_event(&self) -> ResponsesStreamEventOut {
        let mut ev = self.content_part_event(
            EVENT_CONTENT_PART_DONE,
            summary_part("output_text", &self.text),
            true,
        );
        if let Some(part) = &mut ev.part {
            part.text = self.text.clone();
        }
        ev
    }

    fn reasoning_done_event(&self) -> ResponsesStreamEventOut {
        ResponsesStreamEventOut {
            r#type: EVENT_REASONING_SUMMARY_DONE.into(),
            response: None,
            delta: None,
            item: None,
            output_index: Some(self.reasoning_output_index),
            content_index: None,
            summary_index: Some(0),
            item_id: Some(self.reasoning_id()),
            part: Some(summary_part("summary_text", &self.reasoning)),
            input: None,
            // M1：done 事件带顶层聚合 text（真实 Responses schema）
            text: Some(self.reasoning.clone()),
            arguments: None,
        }
    }

    fn reasoning_summary_part_done_event(&self) -> ResponsesStreamEventOut {
        ResponsesStreamEventOut {
            r#type: EVENT_REASONING_SUMMARY_PART_DONE.into(),
            response: None,
            delta: None,
            item: None,
            output_index: Some(self.reasoning_output_index),
            content_index: None,
            summary_index: Some(0),
            item_id: Some(self.reasoning_id()),
            part: Some(summary_part("summary_text", &self.reasoning)),
            input: None,
            text: None,
            arguments: None,
        }
    }

    fn output_item_done_event(&self, output_index: i64, item: ResponsesOutputOut) -> ResponsesStreamEventOut {
        ResponsesStreamEventOut {
            r#type: EVENT_OUTPUT_ITEM_DONE.into(),
            response: None,
            delta: None,
            item: Some(item),
            output_index: Some(output_index),
            content_index: None,
            summary_index: None,
            item_id: None,
            part: None,
            input: None,
            text: None,
            arguments: None,
        }
    }

    pub(crate) fn final_response(&self) -> ResponsesResponseOut {
        let status = self.output_status();
        let mut output: Vec<ResponsesOutputOut> = Vec::with_capacity(self.output_order.len());
        for ref_ in &self.output_order {
            match ref_ {
                OutputRef::Message => output.push(self.message_output(status)),
                OutputRef::Reasoning => output.push(self.reasoning_output(status)),
                OutputRef::Tool(idx) => {
                    if let Some(tool) = self.tools_by_index.get(idx) {
                        // 无名工具不进终态 output（done 事件已丢弃）
                        if !tool.name.is_empty() {
                            output.push(Self::tool_output_item(tool, status));
                        }
                    }
                }
            }
        }
        ResponsesResponseOut {
            id: self.id.clone(),
            object: "response",
            created_at: self.created,
            status: self.status.clone(),
            error: None,
            incomplete_details: self.incomplete_details.clone(),
            instructions: None,
            max_output_tokens: None,
            model: self.model.clone(),
            output,
            parallel_tool_calls: None,
            previous_response_id: None,
            reasoning: None,
            store: None,
            temperature: None,
            tool_choice: None,
            tools: None,
            top_p: None,
            truncation: None,
            usage: self
                .usage
                .as_ref()
                .map(super::dto::usage_from_chat),
            user: None,
            metadata: None,
        }
        // P1-3：终态事件 usage 恒为对象（上游未给 usage 时零值填充，客户端
        // 从 response.completed 读计费不再遇 null；非终态 created 保持真实 API
        // 的 null 形态）
        .with_zero_usage()
    }

    fn message_output(&self, status: &str) -> ResponsesOutputOut {
        let mut item = output_item(
            "message",
            self.message_id(),
            status,
            vec![ResponsesOutputContentOut {
                r#type: "output_text".into(),
                text: self.text.clone(),
                refusal: String::new(),
                annotations: empty_annotations(),
            }],
        );
        item.role = "assistant".into();
        item
    }

    fn reasoning_output(&self, status: &str) -> ResponsesOutputOut {
        // 真实 Responses 形态：reasoning item 正文在 summary[]（非 content[]）
        let mut item = output_item("reasoning", self.reasoning_id(), status, Vec::new());
        item.summary = vec![summary_part("summary_text", &self.reasoning)];
        item
    }

    /// function_call / custom_tool_call（M6）/ web_search_call（四-4 桥接）/
    /// tool_search_call（P0-4）/ namespace function_call（P0-4）输出 item
    fn tool_output_item(tool: &ChatTool, status: &str) -> ResponsesOutputOut {
        let reasoning_content = (!tool.reasoning_content.trim().is_empty())
            .then(|| tool.reasoning_content.trim().to_string());
        if tool.bridged {
            // 四-4：hosted web_search 桥接 → web_search_call item
            //（query 从累积参数 JSON 解析，done 时完整；无 call_id/name/arguments）
            return ResponsesOutputOut {
                r#type: "web_search_call".into(),
                id: tool.id.clone(),
                status: status.to_string(),
                role: String::new(),
                content: Vec::new(),
                summary: Vec::new(),
                quality: String::new(),
                size: String::new(),
                call_id: None,
                name: None,
                namespace: None,
                arguments: None,
                query: parse_web_search_query(&tool.arguments),
                reasoning_content,
                input: None,
                execution: None,
            };
        }
        if tool.custom {
            // M6：请求声明的 custom 工具 → custom_tool_call item
            //（id 带 ctc_ 前缀；input 从降级参数 {"input":…} 还原；无 arguments）
            let canonical_args =
                crate::service::canonical::canonicalize_json_string_if_parseable(&tool.arguments);
            return ResponsesOutputOut {
                r#type: "custom_tool_call".into(),
                id: format!("ctc_{}", tool.id),
                status: status.to_string(),
                role: String::new(),
                content: Vec::new(),
                summary: Vec::new(),
                quality: String::new(),
                size: String::new(),
                call_id: Some(tool.id.clone()),
                name: Some(tool.name.clone()),
                namespace: None,
                arguments: None,
                query: None,
                reasoning_content,
                input: Some(super::dto::custom_tool_input_from_chat_arguments(&canonical_args)),
                execution: None,
            };
        }
        if tool.tool_search {
            // P0-4：tool_search 代理调用 → tool_search_call item
            //（无 id；arguments 解析为对象形态；execution 恒 client）
            let canonical_args =
                crate::service::canonical::canonicalize_json_string_if_parseable(&tool.arguments);
            return ResponsesOutputOut {
                r#type: "tool_search_call".into(),
                id: String::new(),
                status: status.to_string(),
                role: String::new(),
                content: Vec::new(),
                summary: Vec::new(),
                quality: String::new(),
                size: String::new(),
                call_id: Some(tool.id.clone()),
                name: None,
                namespace: None,
                arguments: Some(super::convert_resp::parse_tool_arguments_object(&canonical_args)),
                reasoning_content,
                query: None,
                input: None,
                execution: Some("client".into()),
            };
        }
        ResponsesOutputOut {
            r#type: "function_call".into(),
            // L4：与真实 API id 形态一致（fc_ 前缀；call_id 保持原值可寻址）
            id: format!("fc_{}", tool.id),
            status: status.to_string(),
            role: String::new(),
            content: Vec::new(),
            summary: Vec::new(),
            quality: String::new(),
            size: String::new(),
            call_id: Some(tool.id.clone()),
            // P0-4：namespace 工具还原原始短名 + namespace 字段
            name: Some(
                tool.orig_name
                    .clone()
                    .unwrap_or_else(|| tool.name.clone()),
            ),
            namespace: tool.namespace.clone(),
            arguments: Some(Value::String(
                crate::service::canonical::canonicalize_json_string_if_parseable(&tool.arguments),
            )),
            query: None,
            // L6：思考归属工具调用（cc-switch reasoning_content 字段同款）
            reasoning_content,
            input: None,
            execution: None,
        }
    }

    /// P0-1：工具 item 的事件面 id（与 tool_output_item 的 item.id 完全一致）：
    /// bridged 用裸 id、custom 用 ctc_ 前缀、其余 fc_ 前缀。
    /// function_call_arguments.delta/done 事件必须引用同一 id，客户端才能把
    /// 参数增量归属到对应 output item（真实 API 语义）。
    fn tool_item_id(tool: &ChatTool) -> String {
        if tool.bridged {
            tool.id.clone()
        } else if tool.custom {
            format!("ctc_{}", tool.id)
        } else {
            format!("fc_{}", tool.id)
        }
    }

    fn next_index(&mut self, kind: OutputRef) -> i64 {
        let index = self.next_output_index;
        self.next_output_index += 1;
        self.output_order.push(kind);
        index
    }

    fn output_status(&self) -> &'static str {
        if self.status == "incomplete" {
            "incomplete"
        } else {
            "completed"
        }
    }

    fn message_id(&self) -> String {
        format!("{}_msg_0", self.id)
    }

    fn reasoning_id(&self) -> String {
        format!("{}_reasoning_0", self.id)
    }
}

/// 四-4：从累积的工具参数 JSON 解析 web_search 查询词（尽力而为；
/// 参数未闭合时返回 None，done 时通常完整）
fn parse_web_search_query(arguments: &str) -> Option<String> {
    let v: Value = serde_json::from_str(arguments).ok()?;
    v.get("query").and_then(|q| q.as_str()).map(str::to_string)
}


/// delta.content → 字符串提取已收敛至 dto::delta_content_text（宽松：兼容
/// string 与 content-parts 数组两种上游形态）

/// 上游流内错误帧 → Responses error 对象（cc-switch chat_error_to_response_error 语义）：
/// 兼容 `{"error":{"message"}}` / `{"error":"str"}` / `{"message"}` / 裸字符串
fn extract_stream_error(err: &Value) -> Value {
    let (message, code) = match err {
        Value::String(s) => (s.clone(), None),
        Value::Object(_) => {
            let message = err
                .pointer("/error/message")
                .and_then(|m| m.as_str())
 .or_else(|| err.get("message").and_then(|m| m.as_str()))
                .or_else(|| err.get("detail").and_then(|m| m.as_str()))
                .unwrap_or("upstream stream error")
                .to_string();
            let code = err
                .pointer("/error/code")
                .and_then(|c| c.as_str())
                .or_else(|| err.get("code").and_then(|c| c.as_str()))
                .map(str::to_string);
            (message, code)
        }
        _ => ("upstream stream error".into(), None),
    };
    match code {
        Some(c) => json!({"code": c, "message": message}),
        None => json!({"message": message}),
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 增量 SSE 解析（上游 Chat 流）
// ---------------------------------------------------------------------------

/// Chat SSE 帧增量解析：跨块缓冲，产出完整 `data:` 载荷与 `event:` 名；
/// `[DONE]` 置终止标志。事件以 `\n\n` 或 `\r\n\r\n` 切分（与 proxy::feed_sse 同语义）。
/// M3：保留 event 名——上游以 `event: error` 名报错（data 无 error 字段）时
/// 转换层据此发 failed/error 终态而非静默丢帧。
pub struct ChatSseParser {
    buf: Vec<u8>,
}

/// 解析出的单帧：event 名（可选）+ data 载荷
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

impl Default for ChatSseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatSseParser {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// 追加字节；`out` 收集完整帧；返回是否遇到 `[DONE]`
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<SseEvent>) -> bool {
        self.buf.extend_from_slice(chunk);
        let mut done = false;
        loop {
            let Some(idx) = find_event_end(&self.buf) else {
                break;
            };
            let event: Vec<u8> = self.buf.drain(..idx).collect();
            let text = String::from_utf8_lossy(&event);
            let mut data_lines: Vec<&str> = Vec::new();
            let mut event_name: Option<String> = None;
            for line in text.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    data_lines.push(data.trim());
                } else if let Some(name) = line.strip_prefix("event:") {
                    let name = name.trim();
                    if !name.is_empty() {
                        event_name = Some(name.to_string());
                    }
                }
            }
            if data_lines.is_empty() {
                continue;
            }
            let data = data_lines.join("\n");
            if data == "[DONE]" {
                done = true;
            } else {
                out.push(SseEvent { event: event_name, data });
            }
            if done {
                break;
            }
        }
        // 防御：异常巨大的不完整事件丢弃缓冲（与 proxy::feed_sse 一致）
        if self.buf.len() > 1_000_000 {
            self.buf.clear();
        }
        done
    }
}

fn find_event_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| i + 2)
        .or_else(|| {
            buf.windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|i| i + 4)
        })
}

// ---------------------------------------------------------------------------
// 流转换管线（代理接入点）
// ---------------------------------------------------------------------------

/// 客户端中断/流被丢弃时的兜底记账（与 tail 通过 recorded 标志互斥，只记一次）
struct BillingOnDrop {
    st: AppState,
    meta: UsageMeta,
    recorded: Arc<AtomicBool>,
    state: Arc<Mutex<ChatToResponsesStreamState>>,
    status: u16,
    latency: i64,
}

impl Drop for BillingOnDrop {
    fn drop(&mut self) {
        if !self.recorded.swap(true, Ordering::SeqCst) {
            let st = self.st.clone();
            let meta = self.meta.clone();
            let usage = self.state.lock().billing_usage();
            let status = self.status;
            let latency = self.latency;
            // 尽力而为：客户端中断后按已累积的 usage 记账
            tokio::spawn(async move {
                tracing::warn!(request_id = %meta.request_id, "responses stream dropped before completion; billing best-effort");
                crate::service::usage::record(&st, &meta, usage.as_ref(), status, latency).await;
            });
        }
    }
}

/// 带兜底记账的转换流（BillingOnDrop 与流同生命周期）
pub struct BillingStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send>>,
    _billing: BillingOnDrop,
}

impl Stream for BillingStream {
    type Item = Result<Bytes, Box<dyn Error + Send + Sync>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // BillingStream 全字段 Unpin，get_mut 安全
        self.get_mut().inner.as_mut().poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

/// 上游 Chat SSE → 客户端 Responses SSE：
/// - 逐帧解析 → `ChatToResponsesStreamState` 转换 → `event: {type}\ndata: {json}\n\n` 透传
/// - 遇 `[DONE]` 立即补发终态事件并记账（usage 取自累积 chunk）
/// - 流异常中断由尾帧兜底：补发终态事件 + 记账（usage 未知）；客户端断开由 BillingOnDrop 兜底
pub fn wrap_chat_stream_to_responses(
    st: AppState,
    meta: UsageMeta,
    upstream: impl Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send + 'static,
    latency: i64,
    record_status: u16,
    response_id: String,
    model: String,
    tool_ctx: super::tool_ctx::ToolContext,
) -> BillingStream {
    let recorded = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let state: Arc<Mutex<ChatToResponsesStreamState>> = Arc::new(Mutex::new(
        ChatToResponsesStreamState::new(response_id, model, tool_ctx),
    ));
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
                // H1：上游传输错误/超时 → response.failed 终态事件（而非把 Err
                // 透传进 body 让 hyper 掐断客户端连接；cc-switch :877-884 同款）
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        if !finished.swap(true, Ordering::SeqCst) {
                            let (final_events, usage) = {
                                let mut state = state.lock();
                                let mut events = Vec::new();
                                for ev in &state.record_transport_error(&e.to_string()) {
                                    events.extend_from_slice(sse_frame(ev).as_bytes());
                                }
                                (events, state.billing_usage())
                            };
                            if !recorded.swap(true, Ordering::SeqCst) {
                                crate::service::usage::record(
                                    &st, &meta, usage.as_ref(), record_status, latency,
                                )
                                .await;
                            }
                            return Ok::<Bytes, Box<dyn Error + Send + Sync>>(Bytes::from(
                                final_events,
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
                        let events = state.process_chunk(&cc);
                        for ev in &events {
                            out.extend_from_slice(sse_frame(ev).as_bytes());
                        }
                        failed |= state.has_failed();
                    }
                    (t, failed)
                };
                // [DONE] 或错误帧 → 立即补发终态（错误帧由 finalize 发 response.failed）
                if (terminal || failed) && !finished.swap(true, Ordering::SeqCst) {
                    let (final_events, usage, history) = {
                        let mut state = state.lock();
                        let mut events = Vec::new();
                        for ev in &state.finalize() {
                            events.extend_from_slice(sse_frame(ev).as_bytes());
                        }
                        // P0-1：completed/incomplete 终态 tee 进历史库
                        //（failed 不记：半截输出进历史会污染下一轮上下文）
                        let history = super::history::snapshot_from_events_typed(&state)
                            .and_then(|(id, items)| Some((id, items)));
                        (events, state.billing_usage(), history)
                    };
                    if let Some((id, items)) = history {
                        st.responses_history.record(&id, items);
                    }
                    out.extend_from_slice(&final_events);
                    if !recorded.swap(true, Ordering::SeqCst) {
                        crate::service::usage::record(
                            &st,
                            &meta,
                            usage.as_ref(),
                            record_status,
                            latency,
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
            // 上游未以 [DONE] 正常结束：补发终态事件 + 兜底记账
            if !finished.swap(true, Ordering::SeqCst) {
                let (final_events, usage, history) = {
                    let mut state = state.lock();
                    let mut events = Vec::new();
                    for ev in &state.finalize_truncated() {
                        events.extend_from_slice(sse_frame(ev).as_bytes());
                    }
                    // P0-1：断流但产出完整（finish_reason 已到）的 completed 响应
                    // 同样可入历史；纯 failed 不记
                    let history = super::history::snapshot_from_events_typed(&state);
                    (events, state.billing_usage(), history)
                };
                if let Some((id, items)) = history {
                    st.responses_history.record(&id, items);
                }
                if !recorded.swap(true, Ordering::SeqCst) {
                    crate::service::usage::record(
                        &st,
                        &meta,
                        usage.as_ref(),
                        record_status,
                        latency,
                    )
                    .await;
                }
                Ok::<Bytes, Box<dyn Error + Send + Sync>>(Bytes::from(final_events))
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

/// 上游对 stream 请求返回了完整 JSON 体（未遵守 SSE，如部分网关忽略 stream
/// 参数直接回 Chat completion）→ 用既有状态机合成完整事件流。
/// 无 choices / 带错误体时返回 None（调用方走错误整形分支）。
/// P0-4：`tool_ctx` 与流式主路径同参（custom / namespace / tool_search /
/// web_search 桥接在合成路径同样生效）
pub fn synthesize_sse_from_chat_body(
    body: &[u8],
    response_id: &str,
    model: &str,
    tool_ctx: &super::tool_ctx::ToolContext,
) -> Option<String> {
    let raw = std::str::from_utf8(body).ok()?;
    let chunk = ChatStreamChunk::from_json_str(raw)?;
    if chunk.choices.is_empty() || chunk.error.is_some() {
        return None;
    }
    let mut state = ChatToResponsesStreamState::new(
        response_id.to_string(),
        model.to_string(),
        tool_ctx.clone(),
    );
    let mut out = String::new();
    for ev in state.process_chunk(&chunk) {
        out.push_str(&sse_frame(&ev));
    }
    for ev in state.finalize() {
        out.push_str(&sse_frame(&ev));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use crate::service::responses::dto::{ChatStreamChoice, ChatStreamDelta, FunctionDelta};

    fn chunk(
        id: &str,
        content: &str,
        finish: Option<&str>,
        usage: Option<Value>,
        tool_calls: Vec<ToolCallDelta>,
    ) -> ChatStreamChunk {
        ChatStreamChunk {
            id: id.to_string(),
            model: "stream-model".into(),
            created: Some(1700000000),
            choices: vec![ChatStreamChoice {
                index: 0,
                delta: ChatStreamDelta {
                    role: None,
                    content: Some(Value::String(content.to_string())),
                    reasoning_content: None,
                    reasoning: None,
                    refusal: None,
                    tool_calls,
                },
                finish_reason: finish.map(str::to_string),
            }],
            usage: usage.map(|u| serde_json::from_value(u).expect("usage")),
            error: None,
        }
    }

    #[test]
    fn emits_created_then_text_deltas_then_terminal() {
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "stream-model".into(), crate::service::responses::tool_ctx::ToolContext::default());
        let events = state.process_chunk(&chunk("chatcmpl-1", "Hello", None, None, Vec::new()));
        assert_eq!(events.len(), 5, "created + in_progress + item.added + part.added + text.delta");
        assert_eq!(events[0].r#type, "response.created");
        assert_eq!(events[0].response.as_ref().unwrap().status, "in_progress");
        assert_eq!(events[1].r#type, "response.in_progress");
        assert_eq!(events[2].r#type, "response.output_item.added");
        assert_eq!(events[2].output_index, Some(0));
        assert_eq!(events[3].r#type, "response.content_part.added");
        assert_eq!(events[3].part.as_ref().unwrap().r#type, "output_text");
        assert_eq!(events[4].r#type, "response.output_text.delta");
        assert_eq!(events[4].delta.as_deref(), Some("Hello"));
        assert_eq!(events[4].item_id.as_deref(), Some("resp_fixed_msg_0"));

        let events = state.process_chunk(&chunk("chatcmpl-1", " world", None, None, Vec::new()));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].delta.as_deref(), Some(" world"));

        let usage = json!({"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6});
        let events = state.process_chunk(&chunk(
            "chatcmpl-1",
            "",
            Some("stop"),
            Some(usage),
            Vec::new(),
        ));
        // finish_reason → output_text.done + content_part.done + output_item.done
        assert_eq!(events[0].r#type, "response.output_text.done");
        assert_eq!(events[1].r#type, "response.content_part.done");
        assert_eq!(events[1].part.as_ref().unwrap().text, "Hello world");
        assert_eq!(events[2].r#type, "response.output_item.done");
        assert_eq!(events[2].item.as_ref().unwrap().content[0].text, "Hello world");

        let events = state.finalize();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].r#type, "response.completed");
        let resp = events[0].response.as_ref().unwrap();
        assert_eq!(resp.status, "completed");
        assert_eq!(resp.output[0].content[0].text, "Hello world");
        assert_eq!(resp.usage.as_ref().unwrap().input_tokens, 4);
        assert_eq!(resp.usage.as_ref().unwrap().output_tokens, 2);
        // 幂等
        assert!(state.finalize().is_empty());
    }

    #[test]
    fn emits_reasoning_before_text() {
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into(), crate::service::responses::tool_ctx::ToolContext::default());
        let mut c = chunk("id", "", Some("stop"), None, Vec::new());
        c.choices[0].delta.reasoning_content = Some("Deep thought.".into());
        let events = state.process_chunk(&c);
        assert_eq!(events[2].r#type, "response.output_item.added");
        assert_eq!(events[2].item.as_ref().unwrap().r#type, "reasoning");
        assert_eq!(events[3].r#type, "response.reasoning_summary_part.added");
        assert_eq!(events[4].r#type, "response.reasoning_summary_text.delta");
        assert_eq!(events[4].delta.as_deref(), Some("Deep thought."));
        assert_eq!(events[4].summary_index, Some(0));
        // finish 补发 reasoning done（summary part done + item done）
        assert_eq!(events[5].r#type, "response.reasoning_summary_text.done");
        assert_eq!(events[5].part.as_ref().unwrap().text, "Deep thought.");
        assert_eq!(events[6].r#type, "response.reasoning_summary_part.done");
        assert_eq!(events[6].part.as_ref().unwrap().text, "Deep thought.");
        assert_eq!(events[7].r#type, "response.output_item.done");
        assert_eq!(
            events[7].item.as_ref().unwrap().summary[0].text,
            "Deep thought."
        );
    }

    #[test]
    fn correlates_multi_tool_calls_by_index() {
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into(), crate::service::responses::tool_ctx::ToolContext::default());
        // 两个工具：index 1 先出现（带参数），index 0 后出现（仅名称）
        let events = state.process_chunk(&chunk(
            "id",
            "",
            None,
            None,
            vec![ToolCallDelta {
                index: Some(1),
                id: Some("call_2".into()),
                r#type: Some("function".into()),
                function: Some(FunctionDelta {
                    name: Some("get_weather".into()),
                    arguments: Some("{\"city\":\"Paris\"}".into()),
                }),
            }],
        ));
        assert_eq!(events[2].r#type, "response.output_item.added");
        assert_eq!(events[2].output_index, Some(0));
        assert_eq!(events[2].item.as_ref().unwrap().name.as_deref(), Some("get_weather"));
        assert_eq!(events[3].r#type, "response.function_call_arguments.delta");

        let events = state.process_chunk(&chunk(
            "id",
            "",
            None,
            None,
            vec![ToolCallDelta {
                index: Some(0),
                id: Some("call_1".into()),
                r#type: Some("function".into()),
                function: Some(FunctionDelta {
                    name: Some("get_time".into()),
                    arguments: None,
                }),
            }],
        ));
        assert_eq!(events[0].r#type, "response.output_item.added");
        assert_eq!(events[0].output_index, Some(1));

        // finish → 两个工具按 chat index 升序补发 done
        let events = state.process_chunk(&chunk("id", "", Some("tool_calls"), None, Vec::new()));
        assert_eq!(events[0].r#type, "response.function_call_arguments.done");
        assert_eq!(events[1].r#type, "response.output_item.done");
        assert_eq!(events[2].r#type, "response.function_call_arguments.done");
        assert_eq!(events[3].r#type, "response.output_item.done");

        let events = state.finalize();
        let resp = events[0].response.as_ref().unwrap();
        assert_eq!(resp.output.len(), 2);
        // 输出顺序 = 出现顺序（index1 先出现 → 排在前）
        assert_eq!(resp.output[0].name.as_deref(), Some("get_weather"));
        assert_eq!(resp.output[0].call_id.as_deref(), Some("call_2"));
        assert_eq!(
            resp.output[0].arguments.as_ref().unwrap(),
            &Value::String("{\"city\":\"Paris\"}".into())
        );
        assert_eq!(resp.output[1].name.as_deref(), Some("get_time"));
    }

    #[test]
    fn length_finish_reason_yields_incomplete_terminal() {
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into(), crate::service::responses::tool_ctx::ToolContext::default());
        let _ = state.process_chunk(&chunk("id", "partial", Some("length"), None, Vec::new()));
        let events = state.finalize();
        assert_eq!(events[0].r#type, "response.incomplete");
        let resp = events[0].response.as_ref().unwrap();
        assert_eq!(resp.status, "incomplete");
        assert_eq!(
            resp.incomplete_details.as_ref().unwrap().reason,
            "max_output_tokens"
        );
        // 输出 item 状态同步为 incomplete
        assert_eq!(resp.output[0].status, "incomplete");
    }

    #[test]
    fn reasoning_alias_field_supported() {
        // 部分上游用 `reasoning` 而非 `reasoning_content`（Go GetReasoningContent 双字段语义）
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into(), crate::service::responses::tool_ctx::ToolContext::default());
        let mut c = chunk("id", "", Some("stop"), None, Vec::new());
        c.choices[0].delta.reasoning = Some("Alias thought.".into());
        let events = state.process_chunk(&c);
        assert_eq!(events[4].r#type, "response.reasoning_summary_text.delta");
        assert_eq!(events[4].delta.as_deref(), Some("Alias thought."));
        let events = state.finalize();
        let resp = events[0].response.as_ref().unwrap();
        assert_eq!(resp.output[0].summary[0].text, "Alias thought.");
    }

    #[test]
    fn sse_parser_frames_events_and_done() {
        let mut parser = ChatSseParser::new();
        let mut out = Vec::new();
        let done = parser.push(b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\ndata: [DONE]\n\n", &mut out);
        assert!(done);
        assert_eq!(
            out.iter().map(|e| e.data.as_str()).collect::<Vec<_>>(),
            vec![r#"{"a":1}"#, r#"{"b":2}"#]
        );
        // 跨块切分
        let mut parser = ChatSseParser::new();
        let mut out = Vec::new();
        assert!(!parser.push(b"data: {\"c\"", &mut out));
        assert!(!parser.push(b":3}\n\n", &mut out));
        assert_eq!(
            out.iter().map(|e| e.data.as_str()).collect::<Vec<_>>(),
            vec![r#"{"c":3}"#]
        );
        // 事件名行忽略、注释帧忽略
        let mut parser = ChatSseParser::new();
        let mut out = Vec::new();
        let done = parser.push(b"event: ping\ndata: {}\n\n: keepalive\n\n", &mut out);
        assert!(!done);
        // M3：event 名保留（不再忽略）
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data, "{}");
        assert_eq!(out[0].event.as_deref(), Some("ping"));
    }


    #[test]
    fn sse_frame_format_matches_new_api() {
        let ev = ResponsesStreamEventOut {
            r#type: "response.output_text.delta".into(),
            response: None,
            delta: Some("Hello".into()),
            item: None,
            output_index: Some(0),
            content_index: Some(0),
            summary_index: None,
            item_id: Some("resp_msg_0".into()),
            part: None,
            input: None,
            text: None,
            arguments: None,
        };
        assert_eq!(
            sse_frame(&ev),
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\",\"output_index\":0,\"content_index\":0,\"item_id\":\"resp_msg_0\"}\n\n"
        );
    }

    #[test]
    fn content_parts_array_delta_emits_text() {
        // Bug 回归：部分上游（Gemini 兼容层等）流式 delta.content 为 parts 数组，
        // 旧实现仅取 string 形态 → 全部文本被丢弃 → completed 空输出
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into(), crate::service::responses::tool_ctx::ToolContext::default());

        fn text_deltas(events: &[ResponsesStreamEventOut]) -> Vec<&str> {
            events
                .iter()
                .filter(|e| e.r#type == EVENT_OUTPUT_TEXT_DELTA)
                .filter_map(|e| e.delta.as_deref())
                .collect()
        }

        let mut c = chunk("id", "", None, None, Vec::new());
        c.choices[0].delta.content = Some(json!([{"type": "text", "text": "Hello "}]));
        assert_eq!(text_deltas(&state.process_chunk(&c)), vec!["Hello "]);

        let mut c = chunk("id", "", Some("stop"), None, Vec::new());
        c.choices[0].delta.content = Some(json!([
            {"type": "text", "text": "world"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,xxx"}}
        ]));
        assert_eq!(text_deltas(&state.process_chunk(&c)), vec!["world"]);

        let final_events = state.finalize();
        let resp = final_events[0].response.as_ref().unwrap();
        assert_eq!(resp.output[0].content[0].text, "Hello world");
    }
    /// H4：缺 index 的并行工具帧按 id 区分，不坍缩、不串参
    #[test]
    fn missing_index_with_distinct_ids_keeps_tools_separate() {
        let no_index = |id: &str, name: &str, args: &str| ToolCallDelta {
            index: None,
            id: Some(id.to_string()),
            r#type: Some("function".into()),
            function: Some(FunctionDelta {
                name: Some(name.to_string()),
                arguments: Some(args.to_string()),
            }),
        };
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into(), crate::service::responses::tool_ctx::ToolContext::default());
        let _ = state.process_chunk(&chunk("id", "", None, None, vec![
            no_index("call_a", "read_file", "{\"path\":\"a\"}"),
        ]));
        let events = state.process_chunk(&chunk("id", "", Some("tool_calls"), None, vec![
            no_index("call_b", "exec_command", "{\"cmd\":\"ls\"}"),
        ]));
        // 第二个调用独立 item.added（不覆盖 call_a）
        assert_eq!(events[0].r#type, "response.output_item.added");
        assert_eq!(events[0].item.as_ref().unwrap().name.as_deref(), Some("exec_command"));
        let events = state.finalize();
        let resp = events[0].response.as_ref().unwrap();
        assert_eq!(resp.output.len(), 2, "两个独立工具输出");
        assert_eq!(resp.output[0].call_id.as_deref(), Some("call_a"));
        assert_eq!(resp.output[0].arguments.as_ref().unwrap(), &Value::String("{\"path\":\"a\"}".into()));
        assert_eq!(resp.output[1].call_id.as_deref(), Some("call_b"));
    }

    /// M4：content 里流首 <think> 块分离为 reasoning
    #[test]
    fn inline_think_separated_into_reasoning() {
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into(), crate::service::responses::tool_ctx::ToolContext::default());
        let events = state.process_chunk(&chunk("id", "<think>hmm</think>Answer", Some("stop"), None, Vec::new()));
        let types: Vec<&str> = events.iter().map(|e| e.r#type.as_str()).collect();
        assert!(types.contains(&"response.reasoning_summary_text.delta"), "{types:?}");
        assert!(types.contains(&"response.output_text.delta"), "{types:?}");
        let reasoning = events.iter().find(|e| e.r#type == EVENT_REASONING_SUMMARY_DELTA).unwrap();
        assert_eq!(reasoning.delta.as_deref(), Some("hmm"));
        let text = events.iter().find(|e| e.r#type == EVENT_OUTPUT_TEXT_DELTA).unwrap();
        assert_eq!(text.delta.as_deref(), Some("Answer"));
        let events = state.finalize();
        let resp = events[0].response.as_ref().unwrap();
        assert_eq!(resp.output[0].summary[0].text, "hmm");
        assert_eq!(resp.output[1].content[0].text, "Answer");
    }

    #[test]
    fn synthesize_sse_from_valid_chat_body_and_rejects_error_body() {
        let body = json!({
            "id": "chatcmpl-x", "object": "chat.completion", "model": "m",
            "choices": [{"index": 0,
                "message": {"role": "assistant", "content": "Hello world!"},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
        })
        .to_string()
        .into_bytes();
        let sse = synthesize_sse_from_chat_body(&body, "resp_s", "m", &crate::service::responses::tool_ctx::ToolContext::default()).expect("synthesizable");
        assert!(sse.contains("event: response.created\n"));
        assert!(sse.contains("event: response.output_text.delta\n"));
        assert!(sse.contains("Hello world!"));
        assert!(sse.contains("event: response.completed\n"));
        assert!(sse.contains("\"input_tokens\":3"));
    }

    /// P0-1：function_call_arguments.delta/done 的 item_id 必须与
    /// output_item.added 的 item.id 同前缀（fc_）——真实 API 语义：客户端按
    /// item_id↔item.id 关联参数增量
    #[test]
    fn args_events_use_prefixed_item_id() {
        let mut state = ChatToResponsesStreamState::new(
            "resp_fixed".into(),
            "m".into(),
            crate::service::responses::tool_ctx::ToolContext::default(),
        );
        let c = chunk(
            "id",
            "",
            Some("tool_calls"),
            None,
            vec![ToolCallDelta {
                index: Some(0),
                id: Some("call_1".into()),
                r#type: Some("function".into()),
                function: Some(FunctionDelta {
                    name: Some("f".into()),
                    arguments: Some("{\"a\":1}".into()),
                }),
            }],
        );
        let events = state.process_chunk(&c);
        let added = events
            .iter()
            .find(|e| e.r#type == "response.output_item.added")
            .expect("added emitted");
        assert_eq!(added.item.as_ref().unwrap().id, "fc_call_1");
        let delta = events
            .iter()
            .find(|e| e.r#type == "response.function_call_arguments.delta")
            .expect("args delta emitted");
        assert_eq!(delta.item_id.as_deref(), Some("fc_call_1"));
        let final_events = state.finalize();
        let done = events
            .iter()
            .chain(final_events.iter())
            .find(|e| e.r#type == "response.function_call_arguments.done")
            .expect("args done emitted");
        assert_eq!(done.item_id.as_deref(), Some("fc_call_1"));
    }

    /// 四-3：流式 refusal → output_text（此前被静默丢弃）
    #[test]
    fn refusal_delta_emits_text() {
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into(), crate::service::responses::tool_ctx::ToolContext::default());
        let mut c = chunk("id", "", None, None, Vec::new());
        c.choices[0].delta.refusal = Some("I cannot help with that.".into());
        let events = state.process_chunk(&c);
        let deltas: Vec<_> = events
            .iter()
            .filter(|e| e.r#type == "response.output_text.delta")
            .map(|e| e.delta.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(deltas, vec!["I cannot help with that."]);
    }

    /// L6：工具帧前的思考关闭 reasoning item 并附挂到 function_call item
    #[test]
    fn reasoning_attaches_to_function_call_item() {
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into(), crate::service::responses::tool_ctx::ToolContext::default());
        let mut c = chunk("id", "", None, None, Vec::new());
        c.choices[0].delta.reasoning_content = Some("need a tool".into());
        state.process_chunk(&c);
        let events = state.process_chunk(&chunk(
            "id",
            "",
            None,
            None,
            vec![ToolCallDelta {
                index: Some(0),
                id: Some("call_1".into()),
                r#type: Some("function".into()),
                function: Some(FunctionDelta {
                    name: Some("get_weather".into()),
                    arguments: Some("{}".into()),
                }),
            }],
        ));
        let added = events
            .iter()
            .find(|e| e.r#type == "response.output_item.added" && e.item.as_ref().is_some_and(|i| i.r#type == "function_call"));
        let item = added.and_then(|e| e.item.as_ref()).expect("function_call added");
        assert_eq!(item.reasoning_content.as_deref(), Some("need a tool"));
        // reasoning item 已关闭（done 事件在场）
        assert!(events.iter().any(|e| e.r#type == "response.output_item.done"
            && e.item.as_ref().is_some_and(|i| i.r#type == "reasoning")));
    }

    /// 四-4：桥接工具发 web_search_call item、无参数增量事件、query 解析
    #[test]
    fn bridged_web_search_emits_web_search_call_item() {
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into(), crate::service::responses::tool_ctx::ToolContext::from_request(&serde_json::json!({"tools": [{"type": "web_search"}]})));
        let events = state.process_chunk(&chunk(
            "id",
            "",
            None,
            None,
            vec![ToolCallDelta {
                index: Some(0),
                id: Some("call_ws".into()),
                r#type: Some("function".into()),
                function: Some(FunctionDelta {
                    name: Some("web_search".into()),
                    arguments: Some("{\"query\":\"Rust asyn".into()),
                }),
            }],
        ));
        let added = events
            .iter()
            .find(|e| e.r#type == "response.output_item.added")
            .and_then(|e| e.item.as_ref())
            .expect("added");
        assert_eq!(added.r#type, "web_search_call");
        // 补全参数 → finalize 的 done 事件带 query
        state.process_chunk(&chunk(
            "id",
            "",
            None,
            None,
            vec![ToolCallDelta {
                index: Some(0),
                id: Some("call_ws".into()),
                r#type: Some("function".into()),
                function: Some(FunctionDelta {
                    name: Some("web_search".into()),
                    arguments: Some("c\"}".into()),
                }),
            }],
        ));
        let done_events = state.finalize();
        let done = done_events
            .iter()
            .find(|e| e.r#type == "response.output_item.done")
            .and_then(|e| e.item.as_ref())
            .expect("done");
        assert_eq!(done.r#type, "web_search_call");
        assert_eq!(done.query.as_deref(), Some("Rust async"));
    }
    /// M6：custom 工具流式还原 —— custom_tool_call item + input.delta/done 事件面
    #[test]
    fn custom_tool_stream_emits_input_events() {
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into(), crate::service::responses::tool_ctx::ToolContext::from_request(&serde_json::json!({"tools": [{"type": "custom", "name": "apply_patch"}]})));
        let events = state.process_chunk(&chunk(
            "id",
            "",
            Some("tool_calls"),
            None,
            vec![ToolCallDelta {
                index: Some(0),
                id: Some("call_7".into()),
                r#type: Some("function".into()),
                function: Some(FunctionDelta {
                    name: Some("apply_patch".into()),
                    arguments: Some("{\"input\":\"patch body\"}".into()),
                }),
            }],
        ));
        let types: Vec<&str> = events.iter().map(|e| e.r#type.as_str()).collect();
        assert!(
            !types.contains(&"response.function_call_arguments.delta"),
            "custom 工具不发 arguments delta: {types:?}"
        );
        assert!(types.contains(&"response.custom_tool_call_input.delta"), "{types:?}");
        assert!(types.contains(&"response.custom_tool_call_input.done"), "{types:?}");
        let added = events
            .iter()
            .find(|e| e.r#type == "response.output_item.added")
            .expect("added event");
        let item = added.item.as_ref().expect("item");
        assert_eq!(item.r#type, "custom_tool_call");
        assert_eq!(item.id, "ctc_call_7");
        // added 时参数尚未到齐：input 为空（done/终态时完整还原）
        assert_eq!(item.input.as_deref(), Some(""));
        let done = events
            .iter()
            .find(|e| e.r#type == "response.custom_tool_call_input.done")
            .expect("done event");
        assert_eq!(done.input.as_deref(), Some("patch body"));
        assert_eq!(done.item_id.as_deref(), Some("ctc_call_7"));
        let final_events = state.finalize();
        let completed = final_events
            .iter()
            .find(|e| e.r#type == "response.completed")
            .expect("completed");
        let resp = completed.response.as_ref().expect("response");
        assert_eq!(resp.output[0].r#type, "custom_tool_call");
        assert_eq!(resp.output[0].input.as_deref(), Some("patch body"));
    }
}
