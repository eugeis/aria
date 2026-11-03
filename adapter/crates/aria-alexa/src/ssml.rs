//! Convert plain/Markdown LLM text into something Amazon TTS can say.

/// Wrap arbitrary (already cleaned) content in `<speak>`.
pub fn wrap(inner: &str) -> String {
    format!("<speak>{}</speak>", inner.trim())
}

/// Full pipeline: strip markdown, escape, wrap, truncate.
pub fn speak(text: &str) -> String {
    let cleaned = truncate_sentence(&strip_markdown(text), 1200);
    wrap(&escape_ssml(&cleaned))
}

/// Remove Markdown constructs that would be read awkwardly or unsupported.
pub fn strip_markdown(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_fence = false;
    for line in input.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        // Headings: drop the markers, keep the text.
        let t = t.trim_start_matches('#').trim_start();
        // List / bullet markers.
        let t = t.trim_start_matches(['-', '*', '+']).trim_start();
        let t = t
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches(['.', ')'])
            .trim_start();
        // Table rows: drop separator lines and pipe structure.
        if t.chars().filter(|c| *c == '|').count() >= 2 {
            let cells: Vec<&str> = t.trim_matches('|').split('|').map(str::trim).collect();
            if cells
                .iter()
                .all(|c| c.is_empty() || c.chars().all(|c| c == '-' || c == ':'))
            {
                continue;
            }
            let line = cells.join(", ");
            push_sentence(&mut out, &line);
            continue;
        }
        push_sentence(&mut out, t);
    }
    inline(&out)
}

fn push_sentence(out: &mut String, line: &str) {
    if line.trim().is_empty() {
        if !out.ends_with(' ') && !out.is_empty() {
            out.push(' ');
        }
        return;
    }
    if !out.is_empty() && !out.ends_with(' ') {
        out.push(' ');
    }
    out.push_str(line.trim());
}

/// Inline markdown: code spans, bold/italic, links.
fn inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '`' => {
                // Skip until closing backtick (code span: keep content, drop ticks).
                let mut depth = 1;
                while let Some(&(_, n)) = chars.peek() {
                    chars.next();
                    if n == '`' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
            }
            '|' => {
                // Drop stray table pipes.
            }
            '[' => {
                // [text](url) -> text
                if let Some(close) = find(s, i + 1, ']') {
                    if let Some(paren) = s[close + 1..].find('(') {
                        let url_start = close + 1 + paren;
                        if let Some(url_end) = s[url_start..].find(')') {
                            out.push_str(&s[i + 1..close]);
                            let jump = url_start + url_end - i;
                            // Advance the iterator past the link.
                            for _ in 0..jump {
                                if chars.next().is_none() {
                                    break;
                                }
                            }
                            continue;
                        }
                    }
                }
                out.push(c);
            }
            '*' | '_' => {
                // Emphasis markers: drop them.
            }
            other => {
                let _ = i;
                out.push(other);
            }
        }
    }
    out
}

fn find(s: &str, from: usize, target: char) -> Option<usize> {
    s[from..].find(target).map(|p| from + p)
}

/// Escape SSML special characters.
pub fn escape_ssml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// Truncate to `max` chars at a sentence boundary so TTS stays sane.
pub fn truncate_sentence(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let limited: String = s.chars().take(max).collect();
    for cut in ["\n", ". ", "! ", "? ", "; ", ", "] {
        if let Some(p) = limited.rfind(cut) {
            if p > max / 2 {
                return limited[..p].trim_end().to_string();
            }
        }
    }
    limited.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_code_blocks_and_fences() {
        let src = "Here you go:\n```rust\nfn main() {}\n```\nDone.";
        assert_eq!(strip_markdown(src), "Here you go: Done.");
    }

    #[test]
    fn strips_headings_lists_emphasis_links() {
        let src = "# Title\n- **bold** item\n- [a link](https://x.y) item\n*italic* end";
        let out = strip_markdown(src);
        assert_eq!(out, "Title bold item a link item italic end");
    }

    #[test]
    fn escapes_ssml() {
        assert_eq!(escape_ssml("a < b & c > d"), "a &lt; b &amp; c &gt; d");
    }

    #[test]
    fn speak_wraps_and_limits() {
        let out = speak("Hello **world** <test>");
        assert!(out.starts_with("<speak>"));
        assert!(out.ends_with("</speak>"));
        assert!(out.contains("Hello world &lt;test&gt;"));
        let long = speak(&"word ".repeat(1000));
        assert!(long.chars().count() < 1300);
    }

    #[test]
    fn table_rows_flatten() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |";
        assert_eq!(strip_markdown(src), "a, b 1, 2");
    }
}
