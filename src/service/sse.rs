//! SSE 帧切分与终态判定（`feed_sse` / `ChatSseParser` / `SseErrorTail` 共用）。
//!
//! 与具体 handler 无关：Chat 透传（`proxy::feed_sse`）、Responses 转换
//! （`responses::stream::ChatSseParser`）与上游流收尾（`proxy::SseErrorTail`）
//! 都只依赖这里的帧边界与终态谓词，不再经由 proxy 入口模块转发（R2-F-002）。

use serde_json::Value;

/// 未终结帧的缓冲上限（1 MiB）：超过即丢弃，视为上游在发不可切分的巨型事件
/// （`feed_sse` / `ChatSseParser` / `SseErrorTail` 同一防御口径）
const SSE_FRAME_BUFFER_CAP: usize = 1_000_000;

/// 超大未完成帧被丢弃前读取的前缀上限（64 KiB）：足够读出帧头部的协议信号
/// （`event:` 名 / `data:` 顶层 `type`），判定后即丢弃字节，不保留缓冲
const SSE_OVERFLOW_PREFIX_CAP: usize = 64 * 1024;

/// 增量帧扫描：切出 `chunk`（含跨块半帧）里的完整帧交给 `on_frame`；`on_frame`
/// 返回 true 表示命中终止帧，立即停止。无跨块半帧（`buf` 为空）时**原地**扫描
/// `chunk`，只把未终止的尾帧复制进 `buf`（R2-F-003）；`buf` 非空时追加后按
/// 游标扫描、消费过的字节只做**一次** `drain`（R2-PERF-01）。
/// `feed_sse` / `ChatSseParser` 用它，不关心超大帧的终态信号。
pub(crate) fn scan_frames(
    buf: &mut Vec<u8>,
    chunk: &[u8],
    on_frame: impl FnMut(&[u8]) -> bool,
) -> bool {
    scan_frames_impl(buf, chunk, None, on_frame)
}

/// 同 [`scan_frames`]，额外把"被 `SSE_FRAME_BUFFER_CAP` 丢弃的超大未完成帧
/// 是否带终态信号"并进 `overflow`（`SseErrorTail` 的结算依据，PERF-R3-01）。
pub(crate) fn scan_frames_with_overflow(
    buf: &mut Vec<u8>,
    chunk: &[u8],
    overflow: &mut bool,
    on_frame: impl FnMut(&[u8]) -> bool,
) -> bool {
    scan_frames_impl(buf, chunk, Some(overflow), on_frame)
}

/// 帧扫描主体。两条路径的帧切分与终态语义完全一致，只有"半帧从哪来"不同：
/// - `buf` 为空：直接在 `chunk` 上找终止符，完整帧按 `chunk` 切片交给
///   `on_frame`（不先整块复制）；未终止的尾帧才复制进 `buf`。
/// - `buf` 非空：跨 chunk 半帧必须先拼接，追加后从 `buf.len() - 3` 起找终止符
///   ——上一次**未命中**扫描已消费到缓冲区末尾（`drain` 后残余必为半帧），其内
///   不存在终止符；3 = `\r\n\r\n` 跨块时可能需要的最大回看余量，因此单事件被
///   切成 K 块时不再每块从 offset 0 重扫（O(B²/c) → O(B)）。`on_frame` 仍收到
///   从**帧起点**开始的切片，判定语义与整帧解析一致。
///
/// 两条路径共用同一条不变式：命中终态帧时停止解析并把进度归零（`buf` 清空）,
/// 终态之后的残留字节不再参与解析，后续调用也不会把它们合并进一个 `on_frame`
/// 切片（QC S-001）。
///
/// `overflow` 为 `Some` 时，在丢弃超过 `SSE_FRAME_BUFFER_CAP` 的未完成帧
/// （`clear()` 的前一刻）对至多 `SSE_OVERFLOW_PREFIX_CAP` 字节做一次
/// [`partial_frame_is_terminal`] 保守判定，命中且布尔尚未置位则置位——只保留
/// 布尔、不保留字节，因此第二、第三个被丢弃的帧同样参与判定（QC W-001）。
fn scan_frames_impl(
    buf: &mut Vec<u8>,
    chunk: &[u8],
    mut overflow: Option<&mut bool>,
    mut on_frame: impl FnMut(&[u8]) -> bool,
) -> bool {
    if buf.is_empty() {
        let mut frame_start = 0;
        while let Some(end) = find_event_end(chunk, frame_start) {
            let hit = on_frame(&chunk[frame_start..end]);
            frame_start = end;
            if hit {
                // 终态命中：其后的字节（完整帧与未终止尾帧）都不再解析，
                // `buf` 保持空 = 进度归零（与追加路径的 `clear()` 等价）
                buf.clear();
                return true;
            }
        }
        let tail = &chunk[frame_start..];
        if tail.len() <= SSE_FRAME_BUFFER_CAP {
            buf.extend_from_slice(tail);
        } else {
            mark_overflow(tail, overflow.as_deref_mut());
        }
        return false;
    }

    let mut search = buf.len().saturating_sub(3);
    let mut frame_start = 0;
    buf.extend_from_slice(chunk);
    while let Some(end) = find_event_end(buf, search) {
        let hit = on_frame(&buf[frame_start..end]);
        frame_start = end;
        search = end;
        if hit {
            buf.clear();
            return true;
        }
    }
    buf.drain(..frame_start);
    if buf.len() > SSE_FRAME_BUFFER_CAP {
        mark_overflow(buf, overflow.as_deref_mut());
        buf.clear();
    }
    false
}

/// 丢弃超上限未终止帧前的有界前缀终态判定：`overflow` 为 `None` 的调用方
/// （`feed_sse` / `ChatSseParser`）不关心该信号，直接跳过。
fn mark_overflow(tail: &[u8], overflow: Option<&mut bool>) {
    let Some(flag) = overflow else {
        return;
    };
    if *flag {
        return;
    }
    let take = tail.len().min(SSE_OVERFLOW_PREFIX_CAP);
    *flag = partial_frame_is_terminal(&String::from_utf8_lossy(&tail[..take]));
}

/// 从 `from` 起查找最早的帧终止符（`\n\n` 或 `\r\n\r\n`），返回帧结束下标
/// （含终止符）。半帧（尚无空行）返回 `None`，缓冲留给下一块。两种行尾混用时
/// 各自成帧，不再把相邻两帧合并（R2-BUG-02）。
///
/// 单次前向扫描，遇到第一个终止符即返回：每个游标区间只比较一遍字节，多事件
/// chunk 的解析总量与缓冲长度成线性（R2-PERF-01）。两种终止符起点不可能重合，
/// 因此"先遇到的"即"结束位置最早的"。
pub(crate) fn find_event_end(buf: &[u8], from: usize) -> Option<usize> {
    let rest = buf.get(from..)?;
    let mut i = 0;
    while i + 1 < rest.len() {
        if rest[i] == b'\n' && rest[i + 1] == b'\n' {
            return Some(from + i + 2);
        }
        if i + 3 < rest.len()
            && rest[i] == b'\r'
            && rest[i + 1] == b'\n'
            && rest[i + 2] == b'\r'
            && rest[i + 3] == b'\n'
        {
            return Some(from + i + 4);
        }
        i += 1;
    }
    None
}

/// 终态事件类型集合（Responses / Anthropic 的 JSON 顶层 `type`）
const TERMINAL_EVENT_TYPES: [&str; 5] = [
    "response.completed",
    "response.incomplete",
    "response.failed",
    "response.done",
    "message_stop",
];

/// 文本是否可能含终态字面量：`[DONE]` 哨兵或某个终态事件类型名。终态帧在
/// 序列化 JSON 里必以字面子串出现（键名与类型值不做转义），缺席即可判定不含
/// 终态。字符串值偶含同形子串只会多解析一次（假阳性无害），漏检不可能。
pub(crate) fn contains_terminal_literal(text: &str) -> bool {
    text.contains("[DONE]") || TERMINAL_EVENT_TYPES.iter().any(|t| text.contains(t))
}

/// 帧级终态判定（`feed_sse` 与 `SseErrorTail` 共用，R2-BUG-01 / R3-BUG-01）：
/// 认两条等价通道——
/// - `event:` 行 trim 后属于终态集合（`message_stop` / `response.completed` 等；
///   Anthropic 与 Responses 都可能只用 event 名表达终态、`data:` 无顶层 `type`，
///   只看 data 会漏判 → 健康流被记失败并补发错误帧）；
/// - `data:` 行载荷 trim 后等于 `[DONE]`（冒号后有无空格均可），或 JSON 顶层
///   `type` 属于终态集合。
///
/// 正文（content delta）里出现同名协议字面量不算终态：event 名与顶层 `type`
/// 都按取值比对，不做子串匹配。
///
/// 透传热路径每个完整帧都会调用：先用零解析的字面量预筛短路，普通 content
/// delta 帧不再触发 `serde_json` 解析；正文含同名字面量时多解析一次，仍会被
/// 取值比对否决。
pub(crate) fn frame_is_terminal(text: &str) -> bool {
    if !contains_terminal_literal(text) {
        return false;
    }
    text.lines().any(|line| {
        if let Some(name) = line.strip_prefix("event:") {
            return TERMINAL_EVENT_TYPES.contains(&name.trim());
        }
        line.strip_prefix("data:")
            .is_some_and(|data| json_or_done_is_terminal(data.trim()))
    })
}

/// 单个 `data:` 载荷的终态判定：`[DONE]` 哨兵或终态 JSON 事件
fn json_or_done_is_terminal(data: &str) -> bool {
    if data == "[DONE]" {
        return true;
    }
    serde_json::from_str::<Value>(data)
        .ok()
        .is_some_and(|v| json_frame_is_terminal(&v))
}

/// JSON 帧的顶层 `type` 是否属于终态集合
pub(crate) fn json_frame_is_terminal(v: &Value) -> bool {
    v.get("type")
        .and_then(|t| t.as_str())
        .is_some_and(|ty| TERMINAL_EVENT_TYPES.contains(&ty))
}

/// 被 `SSE_FRAME_BUFFER_CAP` 丢弃的超大未完成帧的**前缀**终态判定
/// （PERF-R3-01）：帧体已被截断（JSON 不可能整体解析），也不能退回子串匹配
/// ——正文里的同名协议字面量会把截断的非终态帧误判成功。只认帧头部的显式
/// 协议信号：
/// - `event:` 行 trim 后属于终态集合；
/// - `data:` 行载荷为 `[DONE]`；
/// - `data:` 行载荷以 `{` 开头且**顶层** `"type"` 字符串值属于终态集合
///   （值闭合后即使后续被截断仍可判定）。
///
/// 无法判定（截断在键名或值内部、前缀里没有 `data:`/`event:` 行）一律返回
/// false，由调用方维持失败结算（fail closed）。
pub(crate) fn partial_frame_is_terminal(text: &str) -> bool {
    if !contains_terminal_literal(text) {
        return false;
    }
    text.lines().any(|line| {
        if let Some(name) = line.strip_prefix("event:") {
            return TERMINAL_EVENT_TYPES.contains(&name.trim());
        }
        let Some(data) = line.strip_prefix("data:") else {
            return false;
        };
        let data = data.trim();
        data == "[DONE]"
            || (data.starts_with('{')
                && top_level_json_string_field(data, "type")
                    .is_some_and(|ty| TERMINAL_EVENT_TYPES.contains(&ty)))
    })
}

/// 读（可能被截断的）JSON 对象里深度 1 的字符串字段值：顶层对象直接成员的
/// `{ "name": "value" }`。`{` / `[` 与 `}` / `]` 维护嵌套深度，字符串与转义
/// 按 token 整体跳过，因此嵌套对象/数组里的同名字段（`response.type` 等）与
/// 正文里的字符串都不会命中。顶层对象闭合仍无该字段 → `None`。
fn top_level_json_string_field<'a>(json: &'a str, name: &str) -> Option<&'a str> {
    let bytes = json.as_bytes();
    let mut i = bytes.iter().position(|b| *b == b'{')? + 1;
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                i += 1;
            }
            b'"' => {
                let (key, next) = json_string_token(json, i)?;
                i = next;
                if depth != 0 {
                    continue; // 嵌套层内的字符串，不是顶层键
                }
                let colon = skip_json_ws(bytes, i);
                if bytes.get(colon) != Some(&b':') {
                    continue; // 顶层非键位的裸字符串
                }
                let value_start = skip_json_ws(bytes, colon + 1);
                if bytes.get(value_start) != Some(&b'"') {
                    continue; // 值不是字符串：不参与判定
                }
                if key == name {
                    return json_string_token(json, value_start).map(|(value, _)| value);
                }
                i = json_string_token(json, value_start)?.1;
            }
            _ => i += 1,
        }
    }
    None
}

/// 读取 `start` 处（必须是 `"`）的一个 JSON 字符串 token：返回原始内文与闭合
/// 引号之后的下标。内文不做转义解码（终态类型名不含转义）；转义按"反斜杠连同
/// 下一字节一起跳过"处理。未闭合（被截断）→ `None`。
fn json_string_token(json: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = json.as_bytes();
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some((&json[start + 1..i], i + 1)),
            _ => i += 1,
        }
    }
    None
}

/// 跳过 JSON 空白（空格 / 制表 / CR / LF）
fn skip_json_ws(bytes: &[u8], mut i: usize) -> usize {
    while matches!(bytes.get(i), Some(&b' ' | &b'\t' | &b'\r' | &b'\n')) {
        i += 1;
    }
    i
}
