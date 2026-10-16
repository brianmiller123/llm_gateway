//! 内联 `<think>...</think>` 检测（cc-switch `InlineThinkState`，
//! streaming_codex_chat.rs:40-52/178-268 移植）。
//!
//! 部分第三方上游不单独发 `reasoning_content`，而是把思考直接混在 `content`
//! 里、以 `<think>` 开头 `</think>` 结尾。直接透传会污染最终答案且计费口径
//! （text vs reasoning token）失真。本状态机只识别**流首**的 think 块：
//! - Detecting：开头缓冲，等确证是 <think> 前缀还是普通文本（跨 chunk 部分标签）
//! - Reasoning：增量下发 reasoning，仅持留可能是 `</think>` 前缀的尾部字节
//! - Text：普通文本直通（think 块之后或无 think 块）
//! 非开头的 `<think>`（第二段起）按普通文本处理（cc-switch 同款语义）。

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    /// 流首：尚未确证是否有 think 前缀
    #[default]
    Detecting,
    /// 已进入 think 块
    Reasoning,
    /// 普通文本（think 块之前确证无 / 之后）
    Text,
}

/// 跨 chunk 的内联 think 分离器。`feed` 返回 (reasoning 增量, text 增量)；
/// 流结束/边界时调用 `flush` 冲刷残留缓冲（未闭合 think 块整体算 reasoning）。
#[derive(Debug, Default)]
pub struct InlineThinkState {
    mode: Mode,
    buffer: String,
}

/// 首块前缀判定：缓冲已可确证是 think 块 / 普通文本 / 还需更多字节
enum PrefixDecision {
    NeedMore,
    Reasoning,
    Text,
}

/// 缓冲开头（忽略前导空白）是否命中 `<think>` 前缀
fn leading_think_prefix_decision(buffer: &str) -> PrefixDecision {
    let trimmed = buffer.trim_start();
    if trimmed.is_empty() {
        return PrefixDecision::NeedMore;
    }
    if THINK_OPEN.starts_with(trimmed) {
        // 还没吃满 <think>（可能跨 chunk 截断）
        return PrefixDecision::NeedMore;
    }
    if trimmed.starts_with(THINK_OPEN) {
        PrefixDecision::Reasoning
    } else {
        PrefixDecision::Text
    }
}

/// 分离缓冲开头的完整 `<think>…</think>` 块：返回 (reasoning, 其后文本)
fn split_leading_think_block(buffer: &str) -> Option<(String, String)> {
    let start = buffer.find(THINK_OPEN)?;
    let after_open = &buffer[start + THINK_OPEN.len()..];
    let end = after_open.find(THINK_CLOSE)?;
    Some((
        after_open[..end].to_string(),
        after_open[end + THINK_CLOSE.len()..].to_string(),
    ))
}

/// 缓冲尾部与 `</think>` 前缀的最长匹配字节数（0..=7）。标签为纯 ASCII，
/// 命中的持留段必为 ASCII 字节，drain 切点必落在字符边界上。
fn partial_close_suffix_len(buffer: &str) -> usize {
    let bytes = buffer.as_bytes();
    let tag = THINK_CLOSE.as_bytes();
    (1..=tag.len().min(bytes.len()))
        .rev()
        .find(|&k| bytes[bytes.len() - k..] == tag[..k])
        .unwrap_or(0)
}

/// 非流式一次性分离（M1）：完整文本中的首个 `<think>…</think>` 块拆为
/// (reasoning, 其后文本)；无完整块（缺闭合标签）时返回 None，调用方按原文本处理。
pub fn split_leading_think(text: &str) -> Option<(String, String)> {
    split_leading_think_block(text)
}

impl InlineThinkState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一个 content 增量；返回本次可安全下发的 (reasoning, text) 增量。
    /// 可能两个都为空（跨 chunk 的部分标签在缓冲中等待确证）。
    pub fn feed(&mut self, delta: &str) -> (Vec<String>, Vec<String>) {
        match self.mode {
            Mode::Text => (Vec::new(), vec![delta.to_string()]),
            Mode::Detecting => {
                self.buffer.push_str(delta);
                match leading_think_prefix_decision(&self.buffer) {
                    PrefixDecision::NeedMore => (Vec::new(), Vec::new()),
                    PrefixDecision::Reasoning => {
                        self.mode = Mode::Reasoning;
                        self.strip_open_tag();
                        self.emit_reasoning()
                    }
                    PrefixDecision::Text => {
                        self.mode = Mode::Text;
                        let text = std::mem::take(&mut self.buffer);
                        (Vec::new(), vec![text])
                    }
                }
            }
            Mode::Reasoning => {
                self.buffer.push_str(delta);
                self.emit_reasoning()
            }
        }
    }

    /// 边界冲刷（finish_reason / [DONE] / 断流收尾）：残留缓冲按当前模式归属。
    pub fn flush(&mut self) -> (Vec<String>, Vec<String>) {
        match self.mode {
            Mode::Text => (Vec::new(), Vec::new()),
            Mode::Detecting => {
                self.mode = Mode::Text;
                let text = std::mem::take(&mut self.buffer);
                if text.is_empty() {
                    (Vec::new(), Vec::new())
                } else {
                    (Vec::new(), vec![text])
                }
            }
            Mode::Reasoning => {
                let buffered = std::mem::take(&mut self.buffer);
                self.mode = Mode::Text;
                // 未闭合 think 块：全部算 reasoning（开标签迁移时已剥离；
                // 完整闭合标签出现过则早已迁移，持留的半截标签不会出现）
                if buffered.is_empty() {
                    (Vec::new(), Vec::new())
                } else {
                    (vec![buffered], Vec::new())
                }
            }
        }
    }

    /// Reasoning 模式增量下发：完整 `</think>` 出现 → 切 Text 并产出
    /// (剩余 reasoning, 其后 text)；否则除「可能是 `</think>` 前缀的尾部字节」
    /// 外全部即时下发（跨 chunk 半标签留待后续判定）。缓冲常态 ≤ 7 字节 +
    /// 当前增量，长思考不再整段滞留、也不再整段重扫。
    fn emit_reasoning(&mut self) -> (Vec<String>, Vec<String>) {
        if let Some(end) = self.buffer.find(THINK_CLOSE) {
            let reasoning = self.buffer.drain(..end).as_str().to_string();
            self.buffer.drain(..THINK_CLOSE.len());
            let answer = std::mem::take(&mut self.buffer);
            self.mode = Mode::Text;
            let mut r = Vec::new();
            if !reasoning.is_empty() {
                r.push(reasoning);
            }
            let mut t = Vec::new();
            if !answer.is_empty() {
                t.push(answer);
            }
            (r, t)
        } else {
            let hold = partial_close_suffix_len(&self.buffer);
            if self.buffer.len() == hold {
                return (Vec::new(), Vec::new());
            }
            let emit = self.buffer.len() - hold;
            let reasoning = self.buffer.drain(..emit).collect();
            (vec![reasoning], Vec::new())
        }
    }

    /// 剥离缓冲的前导空白 + `<think>` 开标签（迁移时已确认 trimmed 以
    /// THINK_OPEN 开头，find 必命中）。
    fn strip_open_tag(&mut self) {
        if let Some(pos) = self.buffer.find(THINK_OPEN) {
            self.buffer.drain(..pos + THINK_OPEN.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(s: &mut InlineThinkState, chunks: &[&str]) -> (Vec<String>, Vec<String>) {
        let mut r = Vec::new();
        let mut t = Vec::new();
        for c in chunks {
            let (mut rr, mut tt) = s.feed(c);
            r.append(&mut rr);
            t.append(&mut tt);
        }
        (r, t)
    }

    #[test]
    fn leading_think_block_split() {
        let mut s = InlineThinkState::new();
        let (r, t) = feed_all(
            &mut s,
            &["<think>step ", "by step</think>", "The answer is 42."],
        );
        assert_eq!(r.concat(), "step by step");
        assert_eq!(t.concat(), "The answer is 42.");
    }

    #[test]
    fn plain_text_passthrough() {
        let mut s = InlineThinkState::new();
        let (r, t) = feed_all(&mut s, &["Hello ", "world"]);
        assert!(r.is_empty());
        assert_eq!(t.concat(), "Hello world");
    }

    #[test]
    fn partial_open_tag_across_chunks() {
        // "<thi" + "nk>" 跨 chunk：不得把部分标签当文本下发
        let mut s = InlineThinkState::new();
        let (r1, t1) = s.feed("<thi");
        assert!(r1.is_empty() && t1.is_empty());
        let (r2, t2) = s.feed("nk>reasoning</think>done");
        assert_eq!(r2.concat(), "reasoning");
        assert_eq!(t2.concat(), "done");
    }
    #[test]
    fn partial_close_tag_across_chunks() {
        let mut s = InlineThinkState::new();
        // 半截 </th 持留，abc 先行下发（增量契约）；ink> 补全后产出 text
        let (r1, t1) = s.feed("<think>abc</th");
        assert_eq!(r1, vec!["abc".to_string()]);
        assert!(t1.is_empty());
        let (r2, t2) = s.feed("ink>tail");
        assert!(r2.is_empty());
        assert_eq!(t2, vec!["tail".to_string()]);
    }

    #[test]
    fn unterminated_block_flushes_as_reasoning() {
        let mut s = InlineThinkState::new();
        let (r_feed, t_feed) = s.feed("<think>half thoughts");
        let (r_flush, t_flush) = s.flush();
        let mut r_all = r_feed;
        r_all.extend(r_flush);
        assert_eq!(r_all.concat(), "half thoughts");
        assert!(t_feed.is_empty() && t_flush.is_empty());
    }

    #[test]
    fn mid_stream_think_tag_is_text() {
        // 非流首的 <think>：按普通文本（cc-switch 只识别 leading 块）
        let mut s = InlineThinkState::new();
        let (r, t) = feed_all(&mut s, &["answer <think>not reasoning</think>"]);
        assert!(r.is_empty());
        assert_eq!(t.concat(), "answer <think>not reasoning</think>");
    }

    #[test]
    fn leading_whitespace_before_tag() {
        let mut s = InlineThinkState::new();
        let (r, t) = feed_all(&mut s, &["  <think>r</think>a"]);
        assert_eq!(r.concat(), "r");
        assert_eq!(t.concat(), "a");
    }

    #[test]
    fn reasoning_emits_incrementally_before_close() {
        // 契约：闭合标签未出现期间，除半截闭合标签候选外即时下发，
        // 不等 </think> 整段滞留（旧行为是全程静默）
        let mut s = InlineThinkState::new();
        let (r1, t1) = s.feed("<think>aaa");
        assert_eq!(r1, vec!["aaa".to_string()]);
        assert!(t1.is_empty());
        let (r2, _) = s.feed("bbb");
        assert_eq!(r2, vec!["bbb".to_string()]);
        let (r3, t3) = s.flush();
        assert!(r3.is_empty());
        assert!(t3.is_empty());
    }

    #[test]
    fn multibyte_reasoning_never_splits_chars() {
        // 多字节 reasoning 跨 chunk：持留判定只匹配 ASCII 标签字节，
        // 不得把 UTF-8 字符撕开（曾会 panic/乱码的边界）
        let (r, t) = feed_all(
            &mut InlineThinkState::new(),
            &["<think>深度思考", "再想一步</think>", "答案"],
        );
        assert_eq!(r.concat(), "深度思考再想一步");
        assert_eq!(t.concat(), "答案");
    }
}
