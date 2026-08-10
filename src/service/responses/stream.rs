//! Chat Completions 流式响应 → Responses API 流式事件（SSE）。
//!
//! 移植自 new-api `relaykit/relayconvert/internal/oai_chat/to_oai_responses_stream_resp.go`。
//! `ChatToResponsesStreamState` 是增量状态机：逐 chunk 消费，产出
//! `response.created / output_item.added / output_text.delta / reasoning_summary_text.delta /
//! function_call_arguments.delta / *.done / output_item.done / response.completed(或 incomplete)` 事件。
//! 工具调用按 Chat `index` 关联；`finish_reason` 到达时先补发 done 事件，流结束（[DONE]）时发终态事件。

use std::collections::HashMap;
use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::stream::{once, Stream, StreamExt};
use parking_lot::Mutex;
use serde_json::Value;

use crate::state::AppState;
use crate::store::usage::UsageMeta;

use super::convert_resp::finish_reason_to_status;
use super::dto::{
    empty_annotations, output_item, sse_frame, ChatStreamChunk, IncompleteDetailsOut,
    ResponsesOutputContentOut, ResponsesOutputOut, ResponsesResponseOut, ResponsesStreamEventOut,
    SummaryPartOut, ToolCallDelta, Usage as DtoUsage,
};

const EVENT_CREATED: &str = "response.created";
const EVENT_COMPLETED: &str = "response.completed";
const EVENT_INCOMPLETE: &str = "response.incomplete";
const EVENT_OUTPUT_ITEM_ADDED: &str = "response.output_item.added";
const EVENT_OUTPUT_ITEM_DONE: &str = "response.output_item.done";
const EVENT_OUTPUT_TEXT_DONE: &str = "response.output_text.done";
const EVENT_OUTPUT_TEXT_DELTA: &str = "response.output_text.delta";
const EVENT_REASONING_SUMMARY_DELTA: &str = "response.reasoning_summary_text.delta";
const EVENT_REASONING_SUMMARY_DONE: &str = "response.reasoning_summary_text.done";
const EVENT_FUNCTION_ARGS_DELTA: &str = "response.function_call_arguments.delta";
const EVENT_FUNCTION_ARGS_DONE: &str = "response.function_call_arguments.done";

/// 输出顺序引用（决定终态 response.output 的排列）
#[derive(Debug, Clone, Copy, PartialEq)]
enum OutputRef {
    Message,
    Reasoning,
    Tool(i64),
}

/// 单个 Chat 工具调用（按 Chat choice.delta.tool_calls 的 index 关联）
#[derive(Debug, Default)]
struct ChatTool {
    output_index: i64,
    id: String,
    name: String,
    arguments: String,
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
    output_order: Vec<OutputRef>,
    text: String,
    reasoning: String,
}

impl ChatToResponsesStreamState {
    pub fn new(id: String, model: String) -> Self {
        Self {
            id,
            model,
            created: now_secs(),
            usage: None,
            status: "completed".into(),
            incomplete_details: None,
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
            output_order: Vec::new(),
            text: String::new(),
            reasoning: String::new(),
        }
    }

    /// 消费一个 Chat chunk，产出 0..n 个 Responses 事件
    pub fn process_chunk(&mut self, chunk: &ChatStreamChunk) -> Vec<ResponsesStreamEventOut> {
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
            if let Some(content) = choice.delta.content.as_ref().and_then(string_of) {
                if !content.is_empty() {
                    events.extend(self.append_text_delta(content));
                }
            }
            for tool_call in &choice.delta.tool_calls {
                events.extend(self.append_tool_call_delta(tool_call));
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

    /// 流结束（[DONE] 或连接断开）时补发终态事件；幂等
    pub fn finalize(&mut self) -> Vec<ResponsesStreamEventOut> {
        if self.finalized {
            return Vec::new();
        }
        self.finalized = true;
        let mut events = self.done_delta_events();
        let resp = self.final_response();
        let event_type = if self.status == "incomplete" {
            EVENT_INCOMPLETE
        } else {
            EVENT_COMPLETED
        };
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
        });
        events
    }

    /// 记账用：状态内 usage 归一化为 store::Usage（双形态回退）
    pub fn billing_usage(&self) -> Option<crate::store::usage::Usage> {
        self.usage.as_ref().map(|u| crate::store::usage::Usage {
            prompt_tokens: Some(u.input()),
            completion_tokens: Some(u.output()),
            input_tokens: None,
            output_tokens: None,
        })
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
                max_output_tokens: 0,
                model: self.model.clone(),
                output: Vec::new(),
                parallel_tool_calls: false,
                previous_response_id: None,
                reasoning: None,
                store: false,
                temperature: 0.0,
                tool_choice: None,
                tools: None,
                top_p: 0.0,
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
        }
    }

    fn append_text_delta(&mut self, delta: &str) -> Vec<ResponsesStreamEventOut> {
        let mut events = Vec::with_capacity(2);
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
            });
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
        });
        events
    }

    fn append_reasoning_delta(&mut self, delta: &str) -> Vec<ResponsesStreamEventOut> {
        let mut events = Vec::with_capacity(2);
        if !self.reasoning_started {
            self.reasoning_started = true;
            self.reasoning_output_index = self.next_index(OutputRef::Reasoning);
            events.push(ResponsesStreamEventOut {
                r#type: EVENT_OUTPUT_ITEM_ADDED.into(),
                response: None,
                delta: None,
                item: Some(output_item(
                    "reasoning",
                    self.reasoning_id(),
                    "in_progress",
                    Vec::new(),
                )),
                output_index: Some(self.reasoning_output_index),
                content_index: None,
                summary_index: None,
                item_id: None,
                part: None,
            });
        }
        self.reasoning.push_str(delta);
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
        });
        events
    }

    fn append_tool_call_delta(&mut self, tool_call: &ToolCallDelta) -> Vec<ResponsesStreamEventOut> {
        let chat_index = tool_call.index.unwrap_or(0);
        let mut events = Vec::with_capacity(2);

        let is_new = !self.tools_by_index.contains_key(&chat_index);
        let output_index = if is_new {
            self.next_index(OutputRef::Tool(chat_index))
        } else {
            self.tools_by_index[&chat_index].output_index
        };
        let tool = self.tools_by_index.entry(chat_index).or_insert_with(|| ChatTool {
            output_index,
            id: tool_call
                .id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("{}_call_{}", self.id, chat_index)),
            name: tool_call
                .function
                .as_ref()
                .and_then(|f| f.name.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_default(),
            arguments: String::new(),
            done: false,
        });
        if is_new {
            // 新工具：先发 output_item.added（Go 语义：in_progress + 空 arguments `""`）
            events.push(ResponsesStreamEventOut {
                r#type: EVENT_OUTPUT_ITEM_ADDED.into(),
                response: None,
                delta: None,
                item: Some(Self::tool_output_item(tool, "in_progress")),
                output_index: Some(tool.output_index),
                content_index: None,
                summary_index: None,
                item_id: None,
                part: None,
            });
        }
        if let Some(id) = tool_call.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            tool.id = id.to_string();
        }
        if let Some(name) = tool_call.function.as_ref().and_then(|f| f.name.as_deref()) {
            let name = name.trim();
            if !name.is_empty() {
                tool.name = name.to_string();
            }
        }
        if let Some(args) = tool_call
            .function
            .as_ref()
            .and_then(|f| f.arguments.as_deref())
        {
            if !args.is_empty() {
                tool.arguments.push_str(args);
                events.push(ResponsesStreamEventOut {
                    r#type: EVENT_FUNCTION_ARGS_DELTA.into(),
                    response: None,
                    delta: Some(args.to_string()),
                    item: None,
                    output_index: Some(tool.output_index),
                    content_index: None,
                    summary_index: None,
                    item_id: Some(tool.id.clone()),
                    part: None,
                });
            }
        }
        events
    }

    /// finish_reason → status（length/content_filter → incomplete）
    fn apply_finish_reason(&mut self, finish_reason: &str) {
        let (status, details) = finish_reason_to_status(finish_reason);
        if status == "incomplete" {
            self.status = status;
            self.incomplete_details = details;
        }
    }

    /// 补发各 output 的 done 事件（finish_reason 到达/终态时；按 *_done 标志幂等）
    fn done_delta_events(&mut self) -> Vec<ResponsesStreamEventOut> {
        let mut events = Vec::new();
        let status = self.output_status();
        if self.text_started && !self.text_done {
            self.text_done = true;
            events.push(self.text_done_event());
            events.push(self.output_item_done_event(
                self.text_output_index,
                self.message_output(status),
            ));
        }
        if self.reasoning_started && !self.reasoning_done {
            self.reasoning_done = true;
            events.push(self.reasoning_done_event());
            events.push(self.output_item_done_event(
                self.reasoning_output_index,
                self.reasoning_output(status),
            ));
        }
        let mut indexes: Vec<i64> = self.tools_by_index.keys().copied().collect();
        indexes.sort_unstable();
        for chat_index in indexes {
            let (output_index, item_id, item) = {
                let tool = self.tools_by_index.get_mut(&chat_index).expect("tool exists");
                if tool.done {
                    continue;
                }
                tool.done = true;
                (
                    tool.output_index,
                    tool.id.clone(),
                    Self::tool_output_item(tool, status),
                )
            };
            events.push(ResponsesStreamEventOut {
                r#type: EVENT_FUNCTION_ARGS_DONE.into(),
                response: None,
                delta: None,
                item: None,
                output_index: Some(output_index),
                content_index: None,
                summary_index: None,
                item_id: Some(item_id),
                part: None,
            });
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
        }
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
            part: Some(SummaryPartOut {
                r#type: "summary_text".into(),
                text: self.reasoning.clone(),
            }),
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
        }
    }

    fn final_response(&self) -> ResponsesResponseOut {
        let status = self.output_status();
        let mut output: Vec<ResponsesOutputOut> = Vec::with_capacity(self.output_order.len());
        for ref_ in &self.output_order {
            match ref_ {
                OutputRef::Message => output.push(self.message_output(status)),
                OutputRef::Reasoning => output.push(self.reasoning_output(status)),
                OutputRef::Tool(idx) => {
                    if let Some(tool) = self.tools_by_index.get(idx) {
                        output.push(Self::tool_output_item(tool, status));
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
            max_output_tokens: 0,
            model: self.model.clone(),
            output,
            parallel_tool_calls: false,
            previous_response_id: None,
            reasoning: None,
            store: false,
            temperature: 0.0,
            tool_choice: None,
            tools: None,
            top_p: 0.0,
            truncation: None,
            usage: self
                .usage
                .as_ref()
                .map(super::dto::usage_from_chat),
            user: None,
            metadata: None,
        }
    }

    fn message_output(&self, status: &str) -> ResponsesOutputOut {
        let mut item = output_item(
            "message",
            self.message_id(),
            status,
            vec![ResponsesOutputContentOut {
                r#type: "output_text".into(),
                text: self.text.clone(),
                annotations: empty_annotations(),
            }],
        );
        item.role = "assistant".into();
        item
    }

    fn reasoning_output(&self, status: &str) -> ResponsesOutputOut {
        output_item(
            "reasoning",
            self.reasoning_id(),
            status,
            vec![ResponsesOutputContentOut {
                r#type: "summary_text".into(),
                text: self.reasoning.clone(),
                annotations: empty_annotations(),
            }],
        )
    }

    /// function_call 输出 item（added 用 in_progress，done 用终态）
    fn tool_output_item(tool: &ChatTool, status: &str) -> ResponsesOutputOut {
        ResponsesOutputOut {
            r#type: "function_call".into(),
            id: tool.id.clone(),
            status: status.to_string(),
            role: String::new(),
            content: Vec::new(),
            quality: String::new(),
            size: String::new(),
            call_id: Some(tool.id.clone()),
            name: Some(tool.name.clone()),
            arguments: Some(Value::String(tool.arguments.clone())),
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

/// delta.content → 字符串（仅 string 形态；数组形态忽略）
fn string_of(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s.as_str()),
        _ => None,
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

/// Chat SSE 帧增量解析：跨块缓冲，产出完整 `data:` 载荷；`[DONE]` 置终止标志。
/// 事件以 `\n\n` 或 `\r\n\r\n` 切分（与 proxy::feed_sse 同语义）。
pub struct ChatSseParser {
    buf: Vec<u8>,
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

    /// 追加字节；`out` 收集完整 data 载荷；返回是否遇到 `[DONE]`
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<String>) -> bool {
        self.buf.extend_from_slice(chunk);
        let mut done = false;
        loop {
            let Some(idx) = find_event_end(&self.buf) else {
                break;
            };
            let event: Vec<u8> = self.buf.drain(..idx).collect();
            let text = String::from_utf8_lossy(&event);
            let mut data_lines: Vec<&str> = Vec::new();
            for line in text.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    data_lines.push(data.trim());
                }
            }
            if data_lines.is_empty() {
                continue;
            }
            let data = data_lines.join("\n");
            if data == "[DONE]" {
                done = true;
            } else {
                out.push(data);
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

/// 上游 Chat SSE → 客户端 Responses SSE：
/// - 逐帧解析 → `ChatToResponsesStreamState` 转换 → `event: {type}\ndata: {json}\n\n` 透传
/// - 遇 `[DONE]` 立即补发终态事件并记账（usage 取自累积 chunk）
/// - 流异常中断由尾帧兜底：补发终态事件 + 记账（usage 未知）
pub fn wrap_chat_stream_to_responses(
    st: AppState,
    meta: UsageMeta,
    upstream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    latency: i64,
    record_status: u16,
    response_id: String,
    model: String,
) -> impl Stream<Item = Result<Bytes, Box<dyn Error + Send + Sync>>> + Send {
    let recorded = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let state: Arc<Mutex<ChatToResponsesStreamState>> =
        Arc::new(Mutex::new(ChatToResponsesStreamState::new(response_id, model)));
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
                let chunk = chunk.map_err(|e| -> Box<dyn Error + Send + Sync> { e.into() })?;
                let mut out: Vec<u8> = Vec::new();
                let terminal = {
                    let mut parser = parser.lock();
                    let mut payloads: Vec<String> = Vec::new();
                    let t = parser.push(&chunk, &mut payloads);
                    for data in payloads {
                        let Ok(cc) = serde_json::from_str::<ChatStreamChunk>(&data) else {
                            continue;
                        };
                        let mut state = state.lock();
                        let events = state.process_chunk(&cc);
                        for ev in &events {
                            out.extend_from_slice(sse_frame(ev).as_bytes());
                        }
                    }
                    t
                };
                if terminal && !finished.swap(true, Ordering::SeqCst) {
                    let (final_events, usage) = {
                        let mut state = state.lock();
                        let mut events = Vec::new();
                        for ev in &state.finalize() {
                            events.extend_from_slice(sse_frame(ev).as_bytes());
                        }
                        (events, state.billing_usage())
                    };
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
                let (final_events, usage) = {
                    let mut state = state.lock();
                    let mut events = Vec::new();
                    for ev in &state.finalize() {
                        events.extend_from_slice(sse_frame(ev).as_bytes());
                    }
                    (events, state.billing_usage())
                };
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

    main.chain(tail)
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
                    tool_calls,
                },
                finish_reason: finish.map(str::to_string),
            }],
            usage: usage.map(|u| serde_json::from_value(u).expect("usage")),
        }
    }

    #[test]
    fn emits_created_then_text_deltas_then_terminal() {
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "stream-model".into());
        let events = state.process_chunk(&chunk("chatcmpl-1", "Hello", None, None, Vec::new()));
        assert_eq!(events.len(), 3, "created + item.added + text.delta");
        assert_eq!(events[0].r#type, "response.created");
        assert_eq!(events[0].response.as_ref().unwrap().status, "in_progress");
        assert_eq!(events[1].r#type, "response.output_item.added");
        assert_eq!(events[1].output_index, Some(0));
        assert_eq!(events[2].r#type, "response.output_text.delta");
        assert_eq!(events[2].delta.as_deref(), Some("Hello"));
        assert_eq!(events[2].item_id.as_deref(), Some("resp_fixed_msg_0"));

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
        // finish_reason → output_text.done + output_item.done
        assert_eq!(events[0].r#type, "response.output_text.done");
        assert_eq!(events[1].r#type, "response.output_item.done");
        assert_eq!(events[1].item.as_ref().unwrap().content[0].text, "Hello world");

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
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into());
        let mut c = chunk("id", "", Some("stop"), None, Vec::new());
        c.choices[0].delta.reasoning_content = Some("Deep thought.".into());
        let events = state.process_chunk(&c);
        assert_eq!(events[1].r#type, "response.output_item.added");
        assert_eq!(events[1].item.as_ref().unwrap().r#type, "reasoning");
        assert_eq!(events[2].r#type, "response.reasoning_summary_text.delta");
        assert_eq!(events[2].delta.as_deref(), Some("Deep thought."));
        assert_eq!(events[2].summary_index, Some(0));
        // finish 补发 reasoning done（含 part）
        assert_eq!(events[3].r#type, "response.reasoning_summary_text.done");
        assert_eq!(events[3].part.as_ref().unwrap().text, "Deep thought.");
        assert_eq!(events[4].r#type, "response.output_item.done");
    }

    #[test]
    fn correlates_multi_tool_calls_by_index() {
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into());
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
        assert_eq!(events[1].r#type, "response.output_item.added");
        assert_eq!(events[1].output_index, Some(0));
        assert_eq!(events[1].item.as_ref().unwrap().name.as_deref(), Some("get_weather"));
        assert_eq!(events[2].r#type, "response.function_call_arguments.delta");

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
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into());
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
        let mut state = ChatToResponsesStreamState::new("resp_fixed".into(), "m".into());
        let mut c = chunk("id", "", Some("stop"), None, Vec::new());
        c.choices[0].delta.reasoning = Some("Alias thought.".into());
        let events = state.process_chunk(&c);
        assert_eq!(events[2].r#type, "response.reasoning_summary_text.delta");
        assert_eq!(events[2].delta.as_deref(), Some("Alias thought."));
        let events = state.finalize();
        let resp = events[0].response.as_ref().unwrap();
        assert_eq!(resp.output[0].content[0].text, "Alias thought.");
    }

    #[test]
    fn sse_parser_frames_events_and_done() {
        let mut parser = ChatSseParser::new();
        let mut out = Vec::new();
        let done = parser.push(b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\ndata: [DONE]\n\n", &mut out);
        assert!(done);
        assert_eq!(out, vec![r#"{"a":1}"#, r#"{"b":2}"#]);
        // 跨块切分
        let mut parser = ChatSseParser::new();
        let mut out = Vec::new();
        assert!(!parser.push(b"data: {\"c\"", &mut out));
        assert!(!parser.push(b":3}\n\n", &mut out));
        assert_eq!(out, vec![r#"{"c":3}"#]);
        // 事件名行忽略、注释帧忽略
        let mut parser = ChatSseParser::new();
        let mut out = Vec::new();
        let done = parser.push(b"event: ping\ndata: {}\n\n: keepalive\n\n", &mut out);
        assert!(!done);
        assert_eq!(out, vec!["{}"]);
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
        };
        assert_eq!(
            sse_frame(&ev),
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\",\"output_index\":0,\"content_index\":0,\"item_id\":\"resp_msg_0\"}\n\n"
        );
    }
}
