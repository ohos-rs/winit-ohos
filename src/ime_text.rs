//! Surrounding-text bookkeeping for OHOS IME deletions.
//!
//! The OHOS IME counts deletions in UTF-16 units, while winit's
//! `Ime::DeleteSurrounding` counts UTF-8 bytes.

/// The application's last reported surrounding text, with UTF-8 byte offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SurroundingText {
    text: String,
    cursor: usize,
    anchor: usize,
}

impl SurroundingText {
    pub(crate) fn new(text: String, cursor: usize, anchor: usize) -> Self {
        Self {
            text,
            cursor,
            anchor,
        }
    }

    /// Converts a deletion of UTF-16 units around a collapsed cursor into UTF-8
    /// byte counts and applies it, so repeated deletions stay correct until the
    /// application reports its updated text.
    ///
    /// A count that splits a surrogate pair removes the whole character. Returns
    /// `None` when the byte counts are unknown: the selection is not collapsed,
    /// or the reported excerpt does not cover the deletion.
    pub(crate) fn delete(&mut self, before: usize, after: usize) -> Option<(usize, usize)> {
        if self.cursor != self.anchor {
            return None;
        }
        let cursor = self.cursor;
        let before_bytes = span(self.text.get(..cursor)?.chars().rev(), before)?;
        let after_bytes = span(self.text.get(cursor..)?.chars(), after)?;
        let start = cursor - before_bytes;
        self.text.replace_range(start..cursor + after_bytes, "");
        self.cursor = start;
        self.anchor = start;
        Some((before_bytes, after_bytes))
    }
}

fn span(mut chars: impl Iterator<Item = char>, units: usize) -> Option<usize> {
    let mut covered = 0;
    let mut bytes = 0;
    while covered < units {
        let ch = chars.next()?;
        covered += ch.len_utf16();
        bytes += ch.len_utf8();
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::SurroundingText;

    fn at(text: &str, cursor: usize) -> SurroundingText {
        SurroundingText::new(text.to_owned(), cursor, cursor)
    }

    #[test]
    fn ascii_backspace_and_forward_delete() {
        let mut text = at("abcd", 2);
        assert_eq!(text.delete(1, 0), Some((1, 0)));
        assert_eq!(text, at("acd", 1));
        assert_eq!(text.delete(0, 2), Some((0, 2)));
        assert_eq!(text, at("a", 1));
    }

    #[test]
    fn multibyte_characters_convert_to_bytes() {
        let mut text = at("a中b", 4);
        assert_eq!(text.delete(1, 0), Some((3, 0)));
        assert_eq!(text, at("ab", 1));
    }

    #[test]
    fn surrogate_pairs_are_removed_whole() {
        let mut text = at("a😀b", 5);
        assert_eq!(text.delete(1, 0), Some((4, 0)));
        assert_eq!(text, at("ab", 1));

        let mut text = at("a😀b", 5);
        assert_eq!(text.delete(2, 0), Some((4, 0)));

        let mut text = at("a😀b", 1);
        assert_eq!(text.delete(0, 1), Some((0, 4)));
        assert_eq!(text, at("ab", 1));
    }

    #[test]
    fn repeated_deletions_use_the_updated_text() {
        let mut text = at("中文", 6);
        assert_eq!(text.delete(1, 0), Some((3, 0)));
        assert_eq!(text.delete(1, 0), Some((3, 0)));
        assert_eq!(text, at("", 0));
    }

    #[test]
    fn unknown_extents_are_not_guessed() {
        let mut text = SurroundingText::new("abc".to_owned(), 1, 2);
        assert_eq!(text.delete(1, 0), None);

        let mut text = at("abc", 1);
        assert_eq!(text.delete(2, 0), None);
        assert_eq!(text.delete(0, 3), None);
        assert_eq!(text, at("abc", 1));
    }

    #[test]
    fn empty_deletion_is_a_no_op() {
        let mut text = at("abc", 0);
        assert_eq!(text.delete(0, 0), Some((0, 0)));
        assert_eq!(text, at("abc", 0));
    }
}
