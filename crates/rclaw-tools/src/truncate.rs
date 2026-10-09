//! 工具输出的头尾截断（借鉴 Codex `truncate_middle_chars` / `HeadTailBuffer`）。
//!
//! 命令与脚本的诊断信息（traceback、错误汇总、测试失败列表）几乎都在输出【尾部】，
//! 只保头的截断正好把最关键的部分切掉。头尾截断保留前半段（做了什么）与后半段
//!（结果如何），中段以省略标记替代并标注原始字节数，模型可据此决定用
//! `shell_poll` / 分页 `read` 取回中段。
//!
//! 两层共用同一语义（T2）：
//! - 「单次输出」：[`head_tail_truncate`] 对已完整字符串做一次性截断；
//! - 「持续流」：[`HeadTailBuffer`] 在流式读取时维持头窗口 + 滑动尾窗口，内存有界，
//!   不会为保尾部而无限缓冲。

/// 头尾截断结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadTailOutput {
    /// 截断后的文本；未超限时即原文。超限时形如
    /// `head + "\n...[X bytes omitted, original Y bytes]...\n" + tail`。
    pub text: String,
    /// 原始字节数。
    pub total_bytes: usize,
    /// 是否发生截断。
    pub truncated: bool,
}

impl HeadTailOutput {
    /// 未截断时直接取正文；截断时同样返回 [`Self::text`]（调用方无需分支）。
    pub fn into_text(self) -> String {
        self.text
    }
}

/// 中段省略标记。稳定格式：便于模型识别与测试断言。
pub fn omission_marker(omitted: usize, total: usize) -> String {
    format!("\n...[{omitted} bytes omitted, original {total} bytes]...\n")
}

/// 字节位置是否为 UTF-8 字符边界：位置 0 / 末尾恒为边界；否则该位置的字节
/// 不得是续接字节（10xxxxxx）。`&[u8]` 没有 `str::is_char_boundary`，此处等价实现。
pub(crate) fn is_char_boundary(bytes: &[u8], index: usize) -> bool {
    index == 0 || index >= bytes.len() || (bytes[index] & 0xC0) != 0x80
}

/// 把 UTF-8 边界回退到 `max` 之前最近的字符边界（`max` 必须已 ≤ len）。
pub(crate) fn floor_char_boundary(bytes: &[u8], max: usize) -> usize {
    let mut end = max.min(bytes.len());
    while end > 0 && !is_char_boundary(bytes, end) {
        end -= 1;
    }
    end
}

/// 把起点推进到 `start` 之后最近的字符边界。
pub(crate) fn ceil_char_boundary(bytes: &[u8], start: usize) -> usize {
    let mut begin = start.min(bytes.len());
    while begin < bytes.len() && !is_char_boundary(bytes, begin) {
        begin += 1;
    }
    begin
}

/// 跳过缓冲区开头的续接字节（滑动尾窗口的第一字节可能落在多字节字符中间）。
///
/// 与 [`ceil_char_boundary`] 的区别：位置 0 不视为边界——尾窗口在【原始流】的
/// 0 号位不是真实流起点，必须看字节本身。
pub(crate) fn advance_to_boundary(bytes: &[u8]) -> usize {
    let mut begin = 0usize;
    while begin < bytes.len() && (bytes[begin] & 0xC0) == 0x80 {
        begin += 1;
    }
    begin
}

/// 对完整字符串做头尾截断：保留前 `max/2` 字节与后 `max - max/2` 字节，
/// 中段以 [`omission_marker`] 替代。未超限原样返回。
pub fn head_tail_truncate(content: &str, max_bytes: usize) -> HeadTailOutput {
    // 与 [`HeadTailBuffer::new`] 同口径：0 视为 1，避免空窗口退化。
    let max_bytes = max_bytes.max(1);
    let bytes = content.as_bytes();
    let total = bytes.len();
    if total <= max_bytes {
        return HeadTailOutput {
            text: content.to_string(),
            total_bytes: total,
            truncated: false,
        };
    }
    let head_end = floor_char_boundary(bytes, max_bytes / 2);
    let tail_start = ceil_char_boundary(bytes, total - (max_bytes - head_end));
    // 边界回退/推进后头尾可能重叠（极小 max_bytes 时）；此时退化为只保头，
    // 保证不变式：输出 ≤ max + marker 长度且不 panic。
    if tail_start <= head_end {
        let text = String::from_utf8_lossy(&bytes[..head_end]).into_owned();
        return HeadTailOutput {
            text,
            total_bytes: total,
            truncated: true,
        };
    }
    let head = String::from_utf8_lossy(&bytes[..head_end]).into_owned();
    let tail = String::from_utf8_lossy(&bytes[tail_start..]).into_owned();
    let omitted = total - (head_end + (total - tail_start));
    let text = format!("{head}{}{tail}", omission_marker(omitted, total));
    HeadTailOutput {
        text,
        total_bytes: total,
        truncated: true,
    }
}

/// 流式头尾缓冲：头窗口 + 滑动尾窗口，总内存有界。
///
/// - 前 `head_cap` 字节进入头窗口；
/// - 超出后只保留【最近】`tail_cap` 字节（滑动丢弃更早的中段）；
/// - [`HeadTailBuffer::finish`] 输出与 [`head_tail_truncate`] 相同格式的文本。
///
/// 为什么不全量缓冲后再截断：后台/长命令可能产出 GB 级输出，读取端必须
/// 以 O(max) 内存持续排空管道，避免子进程因管道写满而阻塞。
pub struct HeadTailBuffer {
    head_cap: usize,
    tail_cap: usize,
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
}

impl HeadTailBuffer {
    /// `max_bytes` 为保留下来的正文预算上限（头尾各半）；与 [`head_tail_truncate`]
    /// 的 `max_bytes` 同一口径。`0` 视为 1，避免除零/空窗口。
    pub fn new(max_bytes: usize) -> Self {
        let max_bytes = max_bytes.max(1);
        HeadTailBuffer {
            head_cap: max_bytes / 2,
            tail_cap: max_bytes - max_bytes / 2,
            head: Vec::new(),
            tail: Vec::new(),
            total: 0,
        }
    }

    /// 接收一段原始字节（不要求按 UTF-8 边界切分）。
    pub fn push(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.total = self.total.saturating_add(chunk.len());
        if self.head.len() < self.head_cap {
            let remaining = self.head_cap - self.head.len();
            if chunk.len() <= remaining {
                self.head.extend_from_slice(chunk);
                return;
            }
            self.head.extend_from_slice(&chunk[..remaining]);
            self.push_tail(&chunk[remaining..]);
            return;
        }
        self.push_tail(chunk);
    }

    fn push_tail(&mut self, chunk: &[u8]) {
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > self.tail_cap {
            let excess = self.tail.len() - self.tail_cap;
            self.tail.drain(..excess);
        }
    }

    /// 输出头尾截断文本（含中段省略标记与原始字节数）。
    pub fn finish(self) -> HeadTailOutput {
        let head_end = floor_char_boundary(&self.head, self.head.len());
        let tail_start = advance_to_boundary(&self.tail);
        // kept 按原始字节计（与 head_tail_truncate / read_output_head_tail 同口径）：
        // from_utf8_lossy 会把无效序列替换为更长的 U+FFFD，用文本长度判断会把
        // 「已丢字节」误判为「未截断」。
        let kept = head_end + (self.tail.len() - tail_start);
        let head = String::from_utf8_lossy(&self.head[..head_end]);
        let tail = String::from_utf8_lossy(&self.tail[tail_start..]);
        let truncated = self.total > kept;
        let text = if truncated {
            let omitted = self.total - kept;
            format!("{head}{}{tail}", omission_marker(omitted, self.total))
        } else {
            format!("{head}{tail}")
        };
        HeadTailOutput {
            text,
            total_bytes: self.total,
            truncated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_content_passes_through() {
        let out = head_tail_truncate("hello 世界", 1024);
        assert!(!out.truncated);
        assert_eq!(out.text, "hello 世界");
        assert_eq!(out.total_bytes, "hello 世界".len());
    }

    #[test]
    fn zero_budget_degrades_to_head_only_without_panic() {
        let out = head_tail_truncate("abc", 0);
        assert!(out.truncated);
        // 0 预算：头窗口为空，不 panic 即可。
        assert_eq!(out.total_bytes, 3);
    }

    #[test]
    fn keeps_head_and_tail_with_marker() {
        let content = format!("HEAD{}\nTAIL", "x".repeat(1000));
        let max = 200;
        let out = head_tail_truncate(&content, max);
        assert!(out.truncated);
        assert_eq!(out.total_bytes, content.len());
        assert!(out.text.starts_with("HEAD"), "{}", out.text);
        assert!(out.text.ends_with("\nTAIL"), "{}", out.text);
        assert!(out.text.contains("bytes omitted"), "{}", out.text);
        assert!(out
            .text
            .contains(&format!("original {} bytes", content.len())));
        // 输出必须落在预算附近：头尾合计 ≤ max，加上 marker 的固定开销。
        assert!(out.text.len() < max + 100, "{}", out.text.len());
        // 头尾必须是完整字符（UTF-8 合法性由 String 类型保证）。
    }

    #[test]
    fn truncates_on_char_boundary_for_multibyte_content() {
        // 每个汉字 3 字节；max=10 → 头 5 字节回退到边界 3（保「甲」），
        // 尾起点 30-7=23 落在「辛」中间，推进到 24（保「壬癸」）。
        let content = "甲乙丙丁戊己庚辛壬癸";
        let out = head_tail_truncate(content, 10);
        assert!(out.truncated);
        let marker = omission_marker(30 - 9, 30);
        assert_eq!(out.text, format!("甲{marker}壬癸"), "{}", out.text);
    }

    #[test]
    fn stream_buffer_matches_one_shot_truncate() {
        let content = "error line 1\n".to_string() + &"middle\n".repeat(500) + "\ntraceback: boom";
        let mut buf = HeadTailBuffer::new(256);
        // 模拟乱边界 chunk 切分（落在多字节/行中间）。
        for chunk in content.as_bytes().chunks(7) {
            buf.push(chunk);
        }
        let streamed = buf.finish();
        let one_shot = head_tail_truncate(&content, 256);
        assert_eq!(streamed, one_shot);
    }

    #[test]
    fn stream_buffer_keeps_only_bounded_memory() {
        let mut buf = HeadTailBuffer::new(1024);
        for _ in 0..1000 {
            buf.push(&[b'x'; 4096]);
        }
        let out = buf.finish();
        assert!(out.truncated);
        assert_eq!(out.total_bytes, 4_096_000);
        assert!(out.text.len() < 1024 + 100, "{}", out.text.len());
    }
}
