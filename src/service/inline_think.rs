//! 内联 `<think>...</think>` 检测（cc-switch `InlineThinkState`，
//! streaming_codex_chat.rs:40-52/178-268 移植）。
//!
//! 部分第三方上游不单独发 `reasoning_content`，而是把思考直接混在 `content`
//! 里、以 `<think>` 开头 `</think>` 结尾。直接透传会污染最终答案且计费口径
//! （text vs reasoning token）失真。本状态机只识别**流首**的 think 块：
//! - Detecting：开头缓冲，等确证是 <think> 前缀还是普通文本（跨 chunk 部分标签）
//! - Reasoning：缓冲至 </think> 出现，期间产出 reasoning 增量
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
                        self.drain_complete_think_block()
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
                self.drain_complete_think_block()
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
                if let Some((reasoning, answer)) = split_leading_think_block(&buffered) {
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
                    // 未闭合的 think 块：整体算 reasoning（去掉开标签前缀）
                    let reasoning = buffered
                        .strip_prefix(THINK_OPEN)
                        .map(str::to_string)
                        .unwrap_or(buffered);
                    if reasoning.is_empty() {
                        (Vec::new(), Vec::new())
                    } else {
                        (vec![reasoning], Vec::new())
                    }
                }
            }
        }
    }

    /// Reasoning 模式下尝试分离完整 think 块：命中则切换 Text 并产出两段增量
    fn drain_complete_think_block(&mut self) -> (Vec<String>, Vec<String>) {
        let Some((reasoning, answer)) = split_leading_think_block(&self.buffer) else {
            return (Vec::new(), Vec::new());
        };
        self.mode = Mode::Text;
        self.buffer.clear();
        let mut r = Vec::new();
        if !reasoning.is_empty() {
            r.push(reasoning);
        }
        let mut t = Vec::new();
        if !answer.is_empty() {
            t.push(answer);
        }
        (r, t)
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
        let _ = s.feed("<think>abc</th");
        let (r, t) = s.feed("ink>tail");
        assert_eq!(r.concat(), "abc");
        assert_eq!(t.concat(), "tail");
    }

    #[test]
    fn unterminated_block_flushes_as_reasoning() {
        let mut s = InlineThinkState::new();
        let _ = s.feed("<think>half thoughts");
        let (r, t) = s.flush();
        assert_eq!(r.concat(), "half thoughts");
        assert!(t.is_empty());
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
}
