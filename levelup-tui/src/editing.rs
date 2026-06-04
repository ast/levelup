//! Readline/Emacs query editing on a `(String, cursor)` pair. Tools call these
//! from their key handlers; each returns whether the text changed (so the
//! caller can decide whether to re-run its search). The keymap — which key maps
//! to which op — stays per-tool.

/// Byte offset of the previous char boundary (or 0). Keeps the cursor on a
/// UTF-8 boundary.
pub fn prev_offset(s: &str, pos: usize) -> usize {
    s[..pos]
        .chars()
        .next_back()
        .map_or(0, |c| pos - c.len_utf8())
}

/// Byte offset of the next char boundary (or `s.len()`).
pub fn next_offset(s: &str, pos: usize) -> usize {
    s[pos..]
        .chars()
        .next()
        .map_or(s.len(), |c| pos + c.len_utf8())
}

/// Delete the char before the cursor (Backspace / Ctrl-H).
pub fn delete_back(query: &mut String, cursor: &mut usize) -> bool {
    if *cursor == 0 {
        return false;
    }
    let prev = prev_offset(query, *cursor);
    query.replace_range(prev..*cursor, "");
    *cursor = prev;
    true
}

/// Delete the char under the cursor (Emacs `delete-char` / Ctrl-D).
pub fn delete_forward(query: &mut String, cursor: &mut usize) -> bool {
    if *cursor >= query.len() {
        return false;
    }
    let next = next_offset(query, *cursor);
    query.replace_range(*cursor..next, "");
    true
}

/// Kill from the cursor to end of line (Ctrl-K).
pub fn kill_to_end(query: &mut String, cursor: &mut usize) -> bool {
    if *cursor >= query.len() {
        return false;
    }
    query.truncate(*cursor);
    true
}

/// Kill the whole line (Ctrl-U).
pub fn kill_line(query: &mut String, cursor: &mut usize) -> bool {
    if query.is_empty() {
        return false;
    }
    query.clear();
    *cursor = 0;
    true
}

/// Delete the previous word from the cursor (Ctrl-W), skipping trailing
/// whitespace first so `"foo bar "` deletes `"bar "`.
pub fn delete_word(query: &mut String, cursor: &mut usize) -> bool {
    if *cursor == 0 {
        return false;
    }
    let trimmed = query[..*cursor].trim_end();
    let start = trimmed
        .rfind(char::is_whitespace)
        .map(|i| i + 1)
        .unwrap_or(0);
    if start == *cursor {
        return false;
    }
    query.replace_range(start..*cursor, "");
    *cursor = start;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_and_word_are_multibyte_safe() {
        let (mut q, mut c) = ("café".to_string(), "café".len());
        assert!(delete_back(&mut q, &mut c)); // removes 'é' (2 bytes)
        assert_eq!(q, "caf");
        assert_eq!(c, 3);

        let (mut q, mut c) = ("foo bar ".to_string(), "foo bar ".len());
        assert!(delete_word(&mut q, &mut c));
        assert_eq!(q, "foo ");
    }

    #[test]
    fn kill_helpers() {
        let (mut q, mut c) = ("hello world".to_string(), 5);
        assert!(kill_to_end(&mut q, &mut c));
        assert_eq!(q, "hello");
        assert!(kill_line(&mut q, &mut c));
        assert_eq!((q.as_str(), c), ("", 0));
        assert!(!kill_line(&mut q, &mut c)); // already empty → no change
    }
}
