//! 版面重建与文本后处理：把带几何的「行」聚成段落，并按配置做标点全/半角与去空格。
//!
//! 对应 PixPin 的版面聚类 / `Ocr.LayoutType` / `Ocr.PunctuationType` / `Ocr.RemoveSpaces`：
//! - [`assemble`]：按纵向重叠把文本框归并成「行」，再按行间距聚类成「段落」；
//! - [`apply_punctuation`]：半角 ↔ 全角标点转换；
//! - [`remove_spaces`]：删除空格（保留换行）。

use super::engine::TextLine;
use super::is_cjk;
use crate::config::{OcrLayout, OcrPunctuation};

/// 把行结构重建成文本。
///
/// 有几何信息时先按纵向重叠归并成「行」（同一视觉行内的多个框用空格/无空格拼接），
/// 再按行间距聚类成段落。无几何信息（如回退引擎）时退化为按顺序逐行拼接。
pub fn assemble(lines: &[TextLine], layout: OcrLayout) -> String {
    let text_lines: Vec<&TextLine> = lines.iter().filter(|l| !l.text.trim().is_empty()).collect();
    if text_lines.is_empty() {
        return String::new();
    }
    // 无几何 → 逐行输出
    if text_lines.iter().all(|l| l.rect.is_none()) {
        let joined: Vec<&str> = text_lines.iter().map(|l| l.text.trim()).collect();
        return match layout {
            OcrLayout::Single => joined.join(" "),
            _ => joined.join("\n"),
        };
    }

    // 归并成视觉行。混合输入（部分行无几何）：无几何行不丢弃，附在几何行之后，
    // 否则这些行的文本会静默从输出消失。
    let mut items: Vec<(i32, i32, i32, i32, &TextLine)> = text_lines
        .iter()
        .filter_map(|l| l.rect.map(|(x, y, w, h)| (x, y, w, h, *l)))
        .collect();
    let no_geom: Vec<&TextLine> = text_lines
        .iter()
        .filter(|l| l.rect.is_none())
        .copied()
        .collect();
    if !no_geom.is_empty() {
        crate::diag::log(&format!(
            "[ocr] {} 行无几何信息，附在版面重建结果之后",
            no_geom.len()
        ));
    }
    items.sort_by_key(|r| (r.1, r.0));

    let mut rows: Vec<Vec<(i32, i32, i32, i32, &TextLine)>> = Vec::new();
    for it in items {
        let mut placed = false;
        if let Some(last) = rows.last_mut() {
            let (_, ly, _, lh, _) = last[0];
            let ov_h = (ly + lh).min(it.1 + it.3) - ly.max(it.1);
            let min_h = lh.min(it.3);
            if ov_h > 0 && ov_h * 2 >= min_h {
                last.push(it);
                placed = true;
            }
        }
        if !placed {
            rows.push(vec![it]);
        }
    }
    for r in rows.iter_mut() {
        r.sort_by_key(|x| x.0);
    }

    // 行文本
    let row_text = |row: &Vec<(i32, i32, i32, i32, &TextLine)>| -> String {
        let mut s = String::new();
        for (i, it) in row.iter().enumerate() {
            if i > 0 {
                let prev = &row[i - 1].4.text;
                if need_space(prev, &it.4.text) {
                    s.push(' ');
                }
            }
            s.push_str(it.4.text.trim());
        }
        s
    };

    if layout == OcrLayout::Single {
        let mut parts: Vec<String> = rows.iter().map(row_text).collect();
        parts.extend(no_geom.iter().map(|l| l.text.trim().to_string()));
        return parts.join(" ");
    }

    // 行高中位数 → 段落间距判据
    let mut heights: Vec<i32> = rows.iter().map(|r| r.iter().map(|x| x.3).max().unwrap_or(1)).collect();
    heights.sort_unstable();
    let med_h = heights[heights.len() / 2].max(1) as f32;

    let mut out = String::new();
    let mut prev_bottom: Option<i32> = None;
    for (ri, row) in rows.iter().enumerate() {
        let top = row.iter().map(|x| x.1).min().unwrap_or(0);
        let bottom = row.iter().map(|x| x.1 + x.3).max().unwrap_or(0);
        if ri > 0 {
            let gap = (top - prev_bottom.unwrap_or(top)) as f32;
            out.push('\n');
            if layout == OcrLayout::Multi && gap > 0.6 * med_h {
                out.push('\n');
            }
        }
        out.push_str(&row_text(row));
        prev_bottom = Some(bottom);
    }
    // 无几何信息的行附在最后（不丢弃文本）
    for l in no_geom {
        out.push('\n');
        out.push_str(l.text.trim());
    }
    out
}

/// 相邻文本框拼接时是否需要空格：两侧都是 CJK 时不需要，其余需要。
fn need_space(prev: &str, next: &str) -> bool {
    let a = prev.trim().chars().last();
    let b = next.trim().chars().next();
    match (a, b) {
        (Some(a), Some(b)) => !(is_cjk(a) && is_cjk(b)),
        _ => false,
    }
}

/// 半角 → 全角标点映射（保守：只映射明确的一一对应）。
fn to_full(c: char) -> Option<char> {
    Some(match c {
        ',' => '，',
        '.' => '。',
        '!' => '！',
        '?' => '？',
        ':' => '：',
        ';' => '；',
        '(' => '（',
        ')' => '）',
        '[' => '【',
        ']' => '】',
        '<' => '《',
        '>' => '》',
        _ => return None,
    })
}

/// 全角 → 半角标点映射。
fn to_half(c: char) -> Option<char> {
    Some(match c {
        '，' => ',',
        '。' => '.',
        '！' => '!',
        '？' => '?',
        '：' => ':',
        '；' => ';',
        '（' => '(',
        '）' => ')',
        '【' => '[',
        '】' => ']',
        '《' => '<',
        '》' => '>',
        _ => return None,
    })
}

/// 按配置转换标点。
pub fn apply_punctuation(text: &str, p: OcrPunctuation) -> String {
    match p {
        OcrPunctuation::None => text.to_string(),
        OcrPunctuation::Full => text.chars().map(|c| to_full(c).unwrap_or(c)).collect(),
        OcrPunctuation::Half => text.chars().map(|c| to_half(c).unwrap_or(c)).collect(),
    }
}

/// 删除空格与制表符（含全角空格），保留换行。
pub fn remove_spaces(text: &str) -> String {
    text.chars().filter(|c| !matches!(c, ' ' | '\t' | '\u{3000}')).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, rect: (i32, i32, i32, i32)) -> TextLine {
        TextLine { text: text.into(), conf: 0.9, rect: Some(rect) }
    }

    #[test]
    fn assemble_sorts_rows_top_to_bottom() {
        // 行距正常（间隙小于 0.6×行高），不应产生段落空行
        let lines = vec![line("第二行", (10, 28, 60, 16)), line("第一行", (10, 10, 60, 16))];
        assert_eq!(assemble(&lines, OcrLayout::Multi), "第一行\n第二行");
        assert_eq!(assemble(&lines, OcrLayout::Auto), "第一行\n第二行");
    }

    #[test]
    fn assemble_merges_same_row_boxes() {
        // 同一视觉行的两个框（英文，需空格）
        let lines = vec![line("Hello", (10, 10, 50, 16)), line("World", (70, 11, 50, 16))];
        assert_eq!(assemble(&lines, OcrLayout::Multi), "Hello World");
        // 中文两段不需要空格
        let lines = vec![line("你好", (10, 10, 30, 16)), line("世界", (45, 10, 30, 16))];
        assert_eq!(assemble(&lines, OcrLayout::Multi), "你好世界");
    }

    #[test]
    fn assemble_paragraph_gap_adds_blank_line() {
        // 两行间距远大于行高 → 段落间空行
        let lines = vec![line("段落一", (10, 10, 60, 16)), line("段落二", (10, 100, 60, 16))];
        assert_eq!(assemble(&lines, OcrLayout::Multi), "段落一\n\n段落二");
    }

    #[test]
    fn assemble_single_joins_into_one_line() {
        let lines = vec![line("第一", (10, 10, 30, 16)), line("第二", (10, 40, 30, 16))];
        assert_eq!(assemble(&lines, OcrLayout::Single), "第一 第二");
    }

    #[test]
    fn assemble_without_geometry_falls_back_to_lines() {
        let lines = vec![
            TextLine { text: "a".into(), conf: 0.0, rect: None },
            TextLine { text: "b".into(), conf: 0.0, rect: None },
        ];
        assert_eq!(assemble(&lines, OcrLayout::Auto), "a\nb");
        assert_eq!(assemble(&lines, OcrLayout::Single), "a b");
    }

    #[test]
    fn assemble_mixed_geometry_keeps_ungeoreferenced_lines() {
        // 混合输入：无几何行的文本不得静默丢失（附在几何行之后）
        let lines = vec![
            line("有几何", (10, 10, 60, 16)),
            TextLine { text: "无几何".into(), conf: 0.5, rect: None },
        ];
        let out = assemble(&lines, OcrLayout::Multi);
        assert!(out.contains("有几何") && out.contains("无几何"), "两行都应在输出中: {out:?}");
    }

    #[test]
    fn punctuation_full_and_half_roundtrip() {
        assert_eq!(apply_punctuation("a,b.", OcrPunctuation::Full), "a，b。");
        assert_eq!(apply_punctuation("a，b。", OcrPunctuation::Half), "a,b.");
        assert_eq!(apply_punctuation("a,b.", OcrPunctuation::None), "a,b.");
    }

    #[test]
    fn remove_spaces_keeps_newlines() {
        assert_eq!(remove_spaces("h e l l o\n中 文"), "hello\n中文");
        assert_eq!(remove_spaces("全 角　空 格"), "全角空格");
    }

    #[test]
    fn parse_accepts_known_values() {
        assert_eq!(OcrLayout::parse("multi"), Some(OcrLayout::Multi));
        assert_eq!(OcrLayout::parse("SINGLE"), Some(OcrLayout::Single));
        assert_eq!(OcrLayout::parse("bogus"), None);
        assert_eq!(OcrPunctuation::parse("full"), Some(OcrPunctuation::Full));
        assert_eq!(OcrPunctuation::parse(""), Some(OcrPunctuation::None));
        assert_eq!(OcrPunctuation::parse("x"), None);
    }
}
