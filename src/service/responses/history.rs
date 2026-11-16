//! P0-1：`previous_response_id` 桥接（cc-switch `codex_chat_history.rs` 同款动机）。
//!
//! Responses 客户端（Codex 等）引用服务端会话状态时，网关转换为无状态 Chat
//! 上游无法转发该引用。网关在转换路径上对输出流/非流式响应做 tee：把终态
//! （completed / incomplete）响应的 `output` items 按网关生成的 `resp_<uuid>`
//! 记入进程内 LRU；下一轮请求带 `previous_response_id` 时把记录的 items 前插
//! 到 `input`，再交给既有 input-item 转换链（message / function_call /
//! reasoning / web_search_call item 均已支持）。
//!
//! 边界：
//! - 仅内存态（512 条 LRU + 64 MiB 总字节预算）：重启/过期后引用 → 显式 400
//!   （带修复指引），不静默丢上下文
//! - 多实例部署需会话亲和（或后续接 Redis）；单实例/容器重启语义同 cc-switch
//! - 原生 openai-responses 透传路径不记录：上游自身有状态，`previous_response_id`
//!   原样转发由上游解析（跨方言 failover 时的引用丢失以显式 400 暴露）
//! - failed / 客户端中断的流不记录（半截输出进入历史会污染下一轮上下文）

use std::collections::{HashMap, VecDeque};

use parking_lot::Mutex;
use serde_json::Value;

/// LRU 容量（cc-switch CODEX_CHAT_HISTORY_CAPACITY = 512 同款）
const MAX_ENTRIES: usize = 512;

/// 单条历史（output items 数组序列化后）字节数上限：防巨型工具输出挤爆内存。
/// 超限丢弃该条（下一轮引用将显式 400，而非 OOM）。
const MAX_ENTRY_BYTES: usize = 2 * 1024 * 1024;

/// 总字节预算（默认 64 MiB）：512 条 × 单条 2 MiB 最坏叠加约 1 GiB；进程级
/// 总预算把峰值钉住，超预算从 LRU 队首逐出（最老会话先死，下一轮引用显式 400）。
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// 响应 id 必须 looks like `resp_…`（网关生成形态）才入库，防客户端伪造任意 key
fn plausible_response_id(id: &str) -> bool {
    id.starts_with("resp_") && id.len() >= "resp_".len() + 8 && id.len() <= 128
}

#[derive(Default)]
struct HistoryInner {
    order: VecDeque<String>,
    /// response_id → (序列化字节数, output items)
    map: HashMap<String, (usize, Value)>,
    /// 当前全部条目的序列化字节总数（插入/覆盖/逐出同步维护）
    bytes: usize,
}

impl HistoryInner {
    /// 从 LRU 队首逐出并回收其字节计数；队列空 → false
    fn evict_front(&mut self) -> bool {
        let Some(evicted) = self.order.pop_front() else {
            return false;
        };
        if let Some((bytes, _)) = self.map.remove(&evicted) {
            self.bytes -= bytes;
        }
        true
    }
}

/// 进程内 Responses 会话历史（LRU + 总字节预算）
pub struct ResponseHistoryStore {
    inner: Mutex<HistoryInner>,
    max_entries: usize,
    max_entry_bytes: usize,
    max_total_bytes: usize,
}

impl ResponseHistoryStore {
    pub fn new() -> Self {
        Self::new_with_limits(MAX_ENTRIES, MAX_ENTRY_BYTES, MAX_TOTAL_BYTES)
    }

    /// 注入限额构造（生产经 `new()` 用默认值；单测用小限额验证逐出与计数）
    fn new_with_limits(
        max_entries: usize,
        max_entry_bytes: usize,
        max_total_bytes: usize,
    ) -> Self {
        Self {
            inner: Mutex::new(HistoryInner::default()),
            max_entries,
            max_entry_bytes,
            max_total_bytes,
        }
    }

    /// 记录一条响应的 output items（仅接受非空数组）
    pub fn record(&self, response_id: &str, items: Value) {
        if !plausible_response_id(response_id) {
            return;
        }
        let Some(arr) = items.as_array() else { return };
        if arr.is_empty() {
            return;
        }
        let Ok(serialized) = serde_json::to_vec(&items) else {
            return;
        };
        let bytes = serialized.len();
        if bytes > self.max_entry_bytes {
            tracing::warn!(
                response_id = response_id,
                bytes,
                cap = self.max_entry_bytes,
                "responses history entry exceeds cap; not recorded (next reference will 400)"
            );
            return;
        }
        let mut inner = self.inner.lock();
        // 覆盖：按新旧字节差调整总量（同一 id 不重复计费）
        let previous_bytes = inner.map.get(response_id).map(|(bytes, _)| *bytes);
        match previous_bytes {
            Some(old_bytes) => {
                inner.map.insert(response_id.to_string(), (bytes, items));
                inner.bytes = inner.bytes - old_bytes + bytes;
            }
            None => {
                inner.map.insert(response_id.to_string(), (bytes, items));
                inner.bytes += bytes;
                inner.order.push_back(response_id.to_string());
                // 条目数上限：逐出队首
                while inner.order.len() > self.max_entries {
                    inner.evict_front();
                }
            }
        }
        // 总字节预算：从队首逐出直到总量回到上限内（单条上限已保证可收敛）
        while inner.bytes > self.max_total_bytes && inner.evict_front() {}
        // LRU：命中/新插入统一移到队尾
        if let Some(pos) = inner.order.iter().position(|k| k == response_id) {
            let id = inner.order.remove(pos).expect("position just found");
            inner.order.push_back(id);
        }
    }

    /// 取回某响应的 output items（clone；转换层直接当作 input items 前插）
    pub fn restore(&self, response_id: &str) -> Option<Value> {
        let mut inner = self.inner.lock();
        // LRU 触碰
        if let Some(pos) = inner.order.iter().position(|k| k == response_id) {
            let id = inner.order.remove(pos).expect("position just found");
            inner.order.push_back(id);
        }
        inner.map.get(response_id).map(|(_, items)| items.clone())
    }

    /// 测试/诊断用（生产路径走 restore/record）
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.inner.lock().map.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().map.is_empty()
    }

    /// 当前条目序列化字节总数（单测校验计数精确性）
    #[cfg(test)]
    fn total_bytes(&self) -> usize {
        self.inner.lock().bytes
    }
}

/// 从 finalize 产出的事件中提取历史快照：终态事件（response.completed /
/// response.incomplete）携带完整 response（含 output items）→ Some((id, output))。
/// failed / 非终态 → None。
#[allow(dead_code)]
pub fn snapshot_from_events(
    events: &[super::dto::ResponsesStreamEventOut],
) -> Option<(String, Value)> {
    let terminal = events.iter().find(|ev| {
        matches!(
            ev.r#type.as_str(),
            "response.completed" | "response.incomplete"
        )
    })?;
    let resp = terminal.response.as_ref()?;
    let mut v = serde_json::to_value(resp).ok()?;
    let output = v.get_mut("output")?.take();
    let id = v.get("id")?.as_str()?.to_string();
    Some((id, output))
}

/// 类型化快照：状态机已 finalize 后直接取终态响应（wrap 层在状态锁内调用，
/// 无需依赖事件字节形态）
pub fn snapshot_from_events_typed(
    state: &super::stream::ChatToResponsesStreamState,
) -> Option<(String, Value)> {
    let resp = state.final_response();
    let mut v = serde_json::to_value(resp).ok()?;
    let output = v.get_mut("output")?.take();
    let id = v.get("id")?.as_str()?.to_string();
    Some((id, output))
}

/// 请求侧桥接：`previous_response_id` 命中 → 恢复的 items 前插进 `input` 数组并
/// 移除该字段（后续 validate_unsupported_fields 不再拒绝）；未命中 → 400。
/// `input` 缺失/字符串/单对象形态统一归一为数组（转换层本就按数组消费）。
pub fn enrich_request_with_history(
    store: &ResponseHistoryStore,
    json: &mut Value,
) -> Result<(), String> {
    let Some(pid) = json
        .get("previous_response_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return Ok(());
    };
    let Some(restored) = store.restore(&pid) else {
        return Err(format!(
            "unknown previous_response_id '{pid}': gateway history for this response has \
             expired or was recorded before a restart; resend the full conversation history \
             instead of referencing it (codex clients: disable server-side storage, e.g. \
             'store' 'false' in model args)"
        ));
    };
    let restored_items: Vec<Value> = restored.as_array().cloned().unwrap_or_default();
    let existing = match json.get_mut("input") {
        None => None,
        Some(v) => match v.take() {
            Value::String(s) => Some(vec![serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": s }],
            })]),
            Value::Object(obj) => Some(vec![Value::Object(obj)]),
            Value::Array(arr) => {
                if arr.is_empty() {
                    None
                } else {
                    Some(arr)
                }
            }
            other => Some(vec![other]),
        },
    };
    let mut input: Vec<Value> = restored_items;
    if let Some(existing) = existing {
        input.extend(existing);
    }
    json["input"] = Value::Array(input);
    if let Some(obj) = json.as_object_mut() {
        obj.remove("previous_response_id");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store_with(id: &str, items: Value) -> ResponseHistoryStore {
        let s = ResponseHistoryStore::new();
        s.record(id, items);
        s
    }

    #[test]
    fn record_restore_roundtrip() {
        let s = store_with(
            "resp_abcdef123456",
            json!([{ "type": "message", "role": "assistant", "content": [] }]),
        );
        assert!(s.restore("resp_abcdef123456").is_some());
        assert!(s.restore("resp_unknown00000").is_none());
    }

    #[test]
    fn rejects_implausible_ids_and_empty_items() {
        let s = ResponseHistoryStore::new();
        s.record("not-a-resp-id", json!([{}]));
        s.record("resp_short", json!([{}]));
        s.record("resp_abcdef123456", json!([]));
        s.record("resp_abcdef123456", json!({}));
        assert!(s.is_empty());
    }

    #[test]
    fn lru_eviction_order() {
        let s = ResponseHistoryStore::new();
        for i in 0..MAX_ENTRIES + 5 {
            s.record(&format!("resp_{i:012}"), json!([{ "i": i }]));
        }
        assert_eq!(s.len(), MAX_ENTRIES);
        assert!(s.restore("resp_000000000000").is_none(), "最老条目被逐出");
        assert!(
            s.restore(&format!("resp_{:012}", MAX_ENTRIES + 4))
                .is_some()
        );
    }

    // ---------- 总字节预算（R2-PERF-02） ----------

    /// 计入口径就是 record 的序列化字节数
    fn item_bytes(items: &Value) -> usize {
        serde_json::to_vec(items).unwrap().len()
    }

    /// 造一条体积可控的 output items（text 长度决定序列化字节数）
    fn pad_items(text_len: usize) -> Value {
        json!([{
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "x".repeat(text_len) }]
        }])
    }

    /// 超总预算：从 LRU 队首逐出到预算内，被逐条目 restore 返回 None
    #[test]
    fn total_byte_budget_evicts_oldest_entries() {
        let entry = pad_items(100);
        let per_entry = item_bytes(&entry);
        let s = ResponseHistoryStore::new_with_limits(16, 4096, per_entry * 3);
        for i in 0..4 {
            s.record(&format!("resp_{i:012}"), entry.clone());
        }
        assert_eq!(s.total_bytes(), per_entry * 3, "超预算只逐出队首，剩三份");
        assert!(
            s.restore("resp_000000000000").is_none(),
            "被预算逐出的最老条目不可恢复"
        );
        for i in 1..4 {
            assert!(
                s.restore(&format!("resp_{i:012}")).is_some(),
                "较新条目保留"
            );
        }
    }

    /// 覆盖同一 id：按新旧字节差调整总量，旧份不重复计费，且不新增条目
    #[test]
    fn overwrite_adjusts_total_bytes_without_double_counting() {
        let small = pad_items(10);
        let large = pad_items(200);
        let (small_b, large_b) = (item_bytes(&small), item_bytes(&large));
        let s = ResponseHistoryStore::new_with_limits(8, 4096, 1 << 20);
        s.record("resp_aaaa00000001", small);
        s.record("resp_bbbb00000002", large.clone());
        assert_eq!(s.total_bytes(), small_b + large_b);
        s.record("resp_aaaa00000001", large.clone());
        assert_eq!(
            s.total_bytes(),
            large_b * 2,
            "覆盖后只计新体积，旧份从总量扣除"
        );
        assert_eq!(s.len(), 2, "覆盖不新增条目");
        assert_eq!(s.restore("resp_aaaa00000001").unwrap(), large);
    }

    /// 持续插入：条目数上限与总预算同时成立，计数器不越界
    #[test]
    fn total_bytes_never_exceeds_budget_under_pressure() {
        let cap = 900usize;
        let s = ResponseHistoryStore::new_with_limits(32, 4096, cap);
        for i in 0..50 {
            s.record(&format!("resp_{i:012}"), pad_items(20 + (i * 7) % 40));
            assert!(
                s.total_bytes() <= cap,
                "第 {i} 次插入后计数 {} 越过预算 {cap}",
                s.total_bytes()
            );
            assert!(s.len() <= 32, "条目数上限同时成立");
        }
        assert!(!s.is_empty(), "预算内必须留存条目");
    }

    #[test]
    fn enrich_prepends_and_removes_reference() {
        let s = store_with(
            "resp_prev11111111",
            json!([
                { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "hi" }] },
                { "type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}" }
            ]),
        );
        let mut req = json!({
            "model": "m",
            "previous_response_id": "resp_prev11111111",
            "input": [
                { "type": "function_call_output", "call_id": "call_1", "output": "42" },
                { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "next" }] }
            ]
        });
        enrich_request_with_history(&s, &mut req).expect("enrich ok");
        assert!(req.get("previous_response_id").is_none());
        let input = req["input"].as_array().unwrap();
        assert_eq!(input.len(), 4, "恢复 2 条 + 客户端新增 2 条");
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[3]["role"], "user");
    }

    #[test]
    fn enrich_normalizes_string_and_missing_input() {
        let s = store_with(
            "resp_prev11111111",
            json!([{ "type": "message", "role": "assistant", "content": [] }]),
        );
        // input 为字符串
        let mut req =
            json!({ "model": "m", "previous_response_id": "resp_prev11111111", "input": "hello" });
        enrich_request_with_history(&s, &mut req).expect("enrich ok");
        assert_eq!(req["input"].as_array().unwrap().len(), 2);
        assert_eq!(req["input"][0]["role"], "assistant", "历史在前");
        assert_eq!(req["input"][1]["role"], "user");
        // input 缺失
        let mut req = json!({ "model": "m", "previous_response_id": "resp_prev11111111" });
        enrich_request_with_history(&s, &mut req).expect("enrich ok");
        assert_eq!(req["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn enrich_unknown_reference_errors() {
        let s = ResponseHistoryStore::new();
        let mut req = json!({ "model": "m", "previous_response_id": "resp_gone1111111" });
        let err = enrich_request_with_history(&s, &mut req).expect_err("unknown id");
        assert!(
            err.contains("unknown previous_response_id"),
            "错误信息带修复指引: {err}"
        );
    }

    #[test]
    fn enrich_noop_without_reference() {
        let s = ResponseHistoryStore::new();
        let mut req = json!({ "model": "m", "input": [{ "type": "message", "role": "user", "content": "hi" }] });
        enrich_request_with_history(&s, &mut req).expect("noop ok");
        assert_eq!(req["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn snapshot_extracts_completed_output_only() {
        use super::super::dto::ResponsesStreamEventOut;
        // 构造最小事件：需要 response 字段。用 serde 反序列化构造太繁琐，
        // 直接用 stream 状态机产出的真实事件在 stream.rs 测试；此处测过滤逻辑。
        // （ResponsesStreamEventOut 字段多为 Option，手写构造一个 completed 事件）
        let mut ev = ResponsesStreamEventOut {
            r#type: "response.completed".into(),
            response: None,
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
        };
        assert!(
            snapshot_from_events(std::slice::from_ref(&ev)).is_none(),
            "无 response 载荷"
        );
        ev.r#type = "response.output_text.delta".into();
        assert!(
            snapshot_from_events(std::slice::from_ref(&ev)).is_none(),
            "非终态事件忽略"
        );
        let _ = ev;
    }
}
