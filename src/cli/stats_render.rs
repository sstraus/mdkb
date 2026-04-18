//! ASCII rendering primitives for `mdkb stats`.
//!
//! All functions return `String` so callers can unit-test without a tty.

// ── Bar chart ────────────────────────────────────────────────────────────────

/// Block characters from full to empty, index 0 = full.
const BLOCKS: [char; 8] = ['█', '▇', '▆', '▅', '▄', '▃', '▂', '▁'];

/// Render a horizontal bar of `width` block characters representing `value/max`.
///
/// Returns an empty string when `width == 0`. When `max == 0` every cell is
/// the lightest block.
pub fn bar(value: u64, max: u64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let ratio = if max == 0 { 0.0 } else { value as f64 / max as f64 };
    let filled = (ratio * width as f64).round() as usize;
    let filled = filled.min(width);
    let empty = width - filled;

    let mut s = String::with_capacity(width * 4); // UTF-8 chars up to 4 bytes
    for _ in 0..filled {
        s.push(BLOCKS[0]);
    }
    for _ in 0..empty {
        s.push(' ');
    }
    s
}

/// Render a single-character sparkline cell for `value` relative to `max`.
fn spark_char(value: u32, max: u32) -> char {
    if max == 0 {
        return BLOCKS[7];
    }
    let idx = ((f64::from(value) / f64::from(max)) * 7.0).round() as usize;
    BLOCKS[7 - idx.min(7)]
}

/// Render a sparkline string for a slice of values.
///
/// Each value maps to one of `▁▂▃▄▅▆▇█`. Empty slice returns empty string.
pub fn sparkline(values: &[u32]) -> String {
    if values.is_empty() {
        return String::new();
    }
    let max = *values.iter().max().unwrap_or(&0);
    values.iter().map(|&v| spark_char(v, max)).collect()
}

// ── Box frame ────────────────────────────────────────────────────────────────

/// Render a Unicode box-drawing frame around `body` with an optional `title`.
///
/// `width` is the total outer width including the side borders (minimum 4).
/// Lines in `body` that exceed `width - 2` are truncated with `…`.
pub fn frame(title: &str, body: &str, width: usize) -> String {
    let inner = width.saturating_sub(2).max(1);
    let mut out = String::new();

    // Top border with optional title
    out.push('┌');
    if title.is_empty() {
        for _ in 0..inner {
            out.push('─');
        }
    } else {
        let t = truncate(title, inner.saturating_sub(2));
        let label = format!(" {t} ");
        let label_len = label.chars().count();
        let dashes = inner.saturating_sub(label_len);
        let left = dashes / 2;
        let right = dashes - left;
        for _ in 0..left {
            out.push('─');
        }
        out.push_str(&label);
        for _ in 0..right {
            out.push('─');
        }
    }
    out.push('┐');
    out.push('\n');

    // Body lines
    for line in body.lines() {
        out.push('│');
        let padded = pad_or_truncate(line, inner);
        out.push_str(&padded);
        out.push('│');
        out.push('\n');
    }

    // Bottom border
    out.push('└');
    for _ in 0..inner {
        out.push('─');
    }
    out.push('┘');
    out.push('\n');

    out
}

fn truncate(s: &str, max_chars: usize) -> &str {
    let mut end = s.len();
    let mut count = 0;
    for (i, _) in s.char_indices() {
        if count >= max_chars {
            end = i;
            break;
        }
        count += 1;
    }
    if count < max_chars {
        s
    } else {
        &s[..end]
    }
}

fn pad_or_truncate(s: &str, width: usize) -> String {
    let char_count = s.chars().count();
    if char_count >= width {
        let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        let mut out = s.to_string();
        for _ in 0..(width - char_count) {
            out.push(' ');
        }
        out
    }
}

// ── ANSI style ───────────────────────────────────────────────────────────────

pub mod style {
    macro_rules! ansi_wrap {
        ($name:ident, $code:literal, $reset:literal) => {
            pub fn $name(s: &str, color: bool) -> String {
                if color {
                    format!(concat!("\x1b[", $code, "m{}\x1b[", $reset, "m"), s)
                } else {
                    s.to_string()
                }
            }
        };
    }

    ansi_wrap!(bold, "1", "0");
    ansi_wrap!(dim, "2", "0");
    ansi_wrap!(red, "31", "0");
    ansi_wrap!(green, "32", "0");
    ansi_wrap!(yellow, "33", "0");
    ansi_wrap!(cyan, "36", "0");
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // bar()
    #[test]
    fn bar_full_value_fills_width() {
        assert_eq!(bar(10, 10, 5), "█████");
    }

    #[test]
    fn bar_zero_value_is_spaces() {
        assert_eq!(bar(0, 10, 5), "     ");
    }

    #[test]
    fn bar_half_value_half_filled() {
        let b = bar(5, 10, 10);
        assert_eq!(b.chars().filter(|&c| c == '█').count(), 5);
        assert_eq!(b.chars().filter(|&c| c == ' ').count(), 5);
    }

    #[test]
    fn bar_zero_width_returns_empty() {
        assert_eq!(bar(10, 10, 0), "");
    }

    #[test]
    fn bar_zero_max_returns_spaces() {
        assert_eq!(bar(0, 0, 4), "    ");
    }

    // sparkline()
    #[test]
    fn sparkline_empty_returns_empty() {
        assert_eq!(sparkline(&[]), "");
    }

    #[test]
    fn sparkline_all_same_max_filled() {
        let s = sparkline(&[5, 5, 5]);
        assert!(s.chars().all(|c| c == '█'), "all max → all full blocks");
    }

    #[test]
    fn sparkline_ascending_ends_full() {
        let s = sparkline(&[0, 4, 8]);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars.len(), 3);
        assert_eq!(chars[2], '█', "last (max) must be full block");
        assert!(chars[0] <= chars[2], "ascending order");
    }

    #[test]
    fn sparkline_single_value_is_full_block() {
        assert_eq!(sparkline(&[42]), "█");
    }

    // frame()
    #[test]
    fn frame_empty_title_no_label_in_border() {
        let out = frame("", "hello", 9);
        let top = out.lines().next().unwrap();
        assert!(top.starts_with('┌'));
        assert!(top.ends_with('┐'));
        assert!(!top.contains('h'), "no title text in border");
    }

    #[test]
    fn frame_title_appears_in_top_border() {
        let out = frame("Stats", "body", 20);
        let top = out.lines().next().unwrap();
        assert!(top.contains("Stats"), "title in top border");
    }

    #[test]
    fn frame_body_line_padded_to_inner_width() {
        let out = frame("", "hi", 10);
        let body_line = out.lines().nth(1).unwrap();
        assert!(body_line.starts_with('│'));
        assert!(body_line.ends_with('│'));
        // inner width = 8, "hi" + 6 spaces
        assert_eq!(body_line.chars().count(), 10);
    }

    #[test]
    fn frame_long_body_line_truncated() {
        let long = "a".repeat(20);
        let out = frame("", &long, 10);
        let body_line = out.lines().nth(1).unwrap();
        assert_eq!(body_line.chars().count(), 10);
        assert!(body_line.contains('…'), "truncated with ellipsis");
    }

    #[test]
    fn frame_bottom_border_closes_box() {
        let out = frame("", "x", 8);
        let last = out.lines().last().unwrap();
        assert!(last.starts_with('└'));
        assert!(last.ends_with('┘'));
    }
}
