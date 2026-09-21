//! op 名处理辅助。

use super::normalize_op;

/// 真实记录路径允许的 `op` 最大 UTF-8 字节数。
///
/// 该限制只约束资源占用并移除控制字符；它不检测 PII、secret，也不把输入校验为受控词表。
pub const MAX_OP_BYTES: usize = 128;

/// 将层级路径片段连接为 op 名（跳过空片段）。
#[must_use]
pub fn join_op_segments(segments: &[&str]) -> String {
    segments
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(".")
}

/// 将索引下压到合法 UTF-8 字符边界（不含 `str::floor_char_boundary`，兼容 MSRV 1.88）。
#[must_use]
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// 清理控制字符并限制 op 显示长度（按字节预算截断，落在 UTF-8 字符边界）。
///
/// - 输入先 trim 并移除控制字符；结果为空时回落为 `"_"`。
/// - 未超长：返回清理后的值。
/// - `max_bytes == 0`：空串。
/// - 超长：在 `max_bytes - 1` 字节内落到字符边界后追加 `~`。
///
/// 返回值在所有情况下都满足 `len() <= max_bytes`。该函数不检测 PII、secret，
/// 也不执行 `op` allowlist 校验。
#[must_use]
pub fn truncate_op(op: &str, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return String::new();
    }

    let mut result = String::with_capacity(op.len().min(max_bytes));
    let mut pending_whitespace = String::with_capacity(max_bytes.min(8));
    let mut pending_overflow = false;
    let mut truncated = false;
    let mut started = false;
    for ch in op.chars().filter(|ch| !ch.is_control()) {
        if !started && ch.is_whitespace() {
            continue;
        }
        if ch.is_whitespace() {
            if result
                .len()
                .saturating_add(pending_whitespace.len())
                .saturating_add(ch.len_utf8())
                <= max_bytes
            {
                pending_whitespace.push(ch);
            } else {
                pending_overflow = true;
            }
            continue;
        }
        if pending_overflow
            || result
                .len()
                .saturating_add(pending_whitespace.len())
                .saturating_add(ch.len_utf8())
                > max_bytes
        {
            truncated = true;
            break;
        }
        result.push_str(&pending_whitespace);
        pending_whitespace.clear();
        started = true;
        result.push(ch);
    }

    if result.is_empty() && !truncated {
        result.push('_');
    }

    if truncated {
        let budget = max_bytes.saturating_sub(1);
        let end = floor_char_boundary(&result, budget);
        result.truncate(end);
        result.push('~');
    }

    result
}

/// 判断 op 是否「可观测友好」（非空、无控制字符、长度受限）。
#[must_use]
pub fn is_friendly_op(op: &str) -> bool {
    let op = op.trim();
    !op.is_empty() && op.len() <= MAX_OP_BYTES && !op.chars().any(|c| c.is_control())
}

/// 统计 op 层级深度（以 `.` 分段；空/归一后 `_` 视为 1）。
#[must_use]
pub fn op_depth(op: &str) -> usize {
    let op = normalize_op(op.trim());
    if op.is_empty() || op == "_" {
        return 1;
    }
    op.split('.').filter(|s| !s.is_empty()).count().max(1)
}

/// 将 `op` 规范为真实记录路径使用的有界值。
///
/// 处理包括 trim、移除控制字符、空值回落和 [`MAX_OP_BYTES`] UTF-8 安全字节上限。
/// 这不是 PII/secret 检测，也不是受控词表 allowlist；调用方仍须治理字段来源与基数。
#[must_use]
pub fn sanitize_op(op: &str) -> String {
    truncate_op(op, MAX_OP_BYTES)
}

/// 取 op 的叶子段（最后一段）；无点则整段。
#[must_use]
pub fn op_leaf(op: &str) -> String {
    let op = sanitize_op(op);
    op.rsplit('.').next().unwrap_or("_").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_and_truncate() {
        assert_eq!(join_op_segments(&["a", "", " b ", "c"]), "a.b.c");
        assert_eq!(join_op_segments(&[]), "");
        assert_eq!(truncate_op("short", 10), "short");
        assert_eq!(truncate_op("abcdefghij", 4), "abc~");
        assert_eq!(truncate_op("abcdefghij", 4).len(), 4);
        assert_eq!(truncate_op("", 3), "_");
        assert_eq!(truncate_op("x", 0), "");
        assert_eq!(truncate_op("配", 1), "~");
        assert_eq!(truncate_op("配", 2), "~");
        assert!(is_friendly_op("svc.fetch"));
        assert!(!is_friendly_op(""));
        assert!(!is_friendly_op("a\nb"));
        assert!(!is_friendly_op(&"x".repeat(200)));
    }

    #[test]
    fn floor_char_boundary_edges() {
        assert_eq!(floor_char_boundary("abc", 0), 0);
        assert_eq!(floor_char_boundary("abc", 3), 3);
        assert_eq!(floor_char_boundary("abc", 99), 3);
        // 多字节：索引落在第二字节 → 0
        assert_eq!(floor_char_boundary("配", 1), 0);
        assert_eq!(floor_char_boundary("配", 2), 0);
        assert_eq!(floor_char_boundary("配", 3), 3);
    }

    #[test]
    fn truncate_op_multibyte_no_panic() {
        // 每个汉字 3 字节；旧实现按裸字节切片会 panic。
        let zh = "配置服务";
        assert_eq!(zh.len(), 12);
        for max in [0usize, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13] {
            let t = truncate_op(zh, max);
            assert!(
                t.is_char_boundary(t.len()),
                "result must be valid UTF-8 for max={max}: {t:?}"
            );
            if max == 0 {
                assert!(t.is_empty());
            } else if max < zh.len() {
                assert!(t.ends_with('~'), "max={max} got {t:?}");
                assert!(t.len() <= max, "max={max} got len={} {t:?}", t.len());
            } else {
                assert_eq!(t, zh);
            }
        }
        // 明确边界：max=4 → budget=3 → 一字 + ~
        assert_eq!(truncate_op(zh, 4), "配~");
        assert_eq!(truncate_op(zh, 7), "配置~");
        // 混合 ASCII + 多字节
        let mixed = "api.配置";
        let t = truncate_op(mixed, 6);
        assert!(t.is_char_boundary(t.len()));
        assert!(t.len() <= 6);
    }

    #[test]
    fn join_many_segments() {
        let segs: Vec<&str> = (0..20).map(|_| "p").collect();
        let j = join_op_segments(&segs);
        assert!(j.starts_with("p.p"));
        assert_eq!(j.matches('.').count(), 19);
        for max in [1usize, 2, 8, 64] {
            let t = truncate_op(&j, max);
            assert!(t.len() <= max, "max={max} got {}", t.len());
        }
    }

    #[test]
    fn depth_sanitize_leaf() {
        assert_eq!(op_depth(""), 1);
        assert_eq!(op_depth("a.b.c"), 3);
        assert_eq!(op_depth("  single  "), 1);
        assert_eq!(sanitize_op("a\nb"), "ab");
        assert_eq!(sanitize_op("   "), "_");
        assert_eq!(sanitize_op("\0  api.fetch  \0"), "api.fetch");
        assert_eq!(sanitize_op("\0 \t\r \0"), "_");
        assert_eq!(truncate_op("api      ", 4), "api");
        assert_eq!(truncate_op(&format!("a{}", " ".repeat(200)), 4), "a");
        assert_eq!(truncate_op("  配置  ", 7), "配置");
        assert_eq!(truncate_op("\0 配置服务 \0", 4), "配~");
        assert_eq!(sanitize_op("a \0 b"), "a  b");
        let malicious = format!("{}\nsecret", "配".repeat(80));
        let sanitized = sanitize_op(&malicious);
        assert!(sanitized.len() <= MAX_OP_BYTES);
        assert!(!sanitized.chars().any(char::is_control));
        assert!(!sanitized.contains("secret"));
        assert_eq!(op_leaf("api.orders.create"), "create");
        assert_eq!(op_leaf(""), "_");
        for n in 1..=10 {
            let segs: Vec<String> = (0..n).map(|i| format!("s{i}")).collect();
            let refs: Vec<&str> = segs.iter().map(String::as_str).collect();
            let j = join_op_segments(&refs);
            assert_eq!(op_depth(&j), n);
            assert_eq!(op_leaf(&j), format!("s{}", n - 1));
        }
    }
}
