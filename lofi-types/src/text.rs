/// Truncate to at most `max` characters, preferring a word boundary in
/// the last two fifths of the window.
#[must_use]
pub fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let mut end_byte = 0;
    for (i, (b, _)) in text.char_indices().enumerate() {
        if i == max {
            end_byte = b;
            break;
        }
    }
    let window = &text[..end_byte];
    let cut = window
        .rfind(' ')
        .filter(|&i| i > end_byte * 3 / 5)
        .unwrap_or(end_byte);
    text[..cut].trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::clip;

    #[test]
    fn clip_word_boundary() {
        assert_eq!(clip("hello world foo bar", 11), "hello world");
        assert_eq!(clip("short", 10), "short");
    }

    #[test]
    fn clip_cuts_mid_word_when_no_late_space() {
        assert_eq!(clip("abcdefghij", 4), "abcd");
        assert_eq!(clip("ab cdefghij", 8), "ab cdefg");
    }

    #[test]
    fn clip_respects_multibyte_chars() {
        assert_eq!(clip("ééééé", 3), "ééé");
        assert_eq!(clip("alpha beta", 8), "alpha");
    }
}
