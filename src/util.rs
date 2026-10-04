use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// `1536` -> `1.50 KiB`. Three significant digits, binary units.
pub fn fmt_bytes(n: u64) -> String {
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[unit])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.2} {}", UNITS[unit])
    }
}

pub fn fmt_rate(bps: f64) -> String {
    format!("{}/s", fmt_bytes(bps.max(0.0) as u64))
}

/// Keeps the end of the string, which is the interesting part of a path.
pub fn truncate_left(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut width = 1;
    let mut tail: Vec<char> = Vec::new();
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if width + w > max {
            break;
        }
        width += w;
        tail.push(c);
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

pub fn truncate_right(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut width = 1;
    let mut head = String::new();
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if width + w > max {
            break;
        }
        width += w;
        head.push(c);
    }
    head.push('…');
    head
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes_with_three_significant_digits() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1024), "1.00 KiB");
        assert_eq!(fmt_bytes(1536), "1.50 KiB");
        assert_eq!(fmt_bytes(10 * 1024), "10.0 KiB");
        assert_eq!(fmt_bytes(150 << 20), "150 MiB");
        assert_eq!(fmt_bytes(3 << 40), "3.00 TiB");
        assert_eq!(fmt_bytes(5000 << 40), "5000 TiB");
        assert_eq!(fmt_rate(2048.0), "2.00 KiB/s");
        assert_eq!(fmt_rate(-1.0), "0 B/s");
    }

    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate_left("hello/world.txt", 8), "…rld.txt");
        assert_eq!(truncate_left("short", 8), "short");
        assert_eq!(truncate_left("日本語", 5), "…本語");
        assert_eq!(truncate_left("abc", 0), "");
        assert_eq!(truncate_right("hello world", 6), "hello…");
        assert_eq!(truncate_right("日本語", 4), "日…");
    }
}
