use unicode_width::UnicodeWidthStr;

/// Однострочное поле ввода. Курсор хранится как байтовый индекс
/// и всегда стоит на границе символа UTF-8.
#[derive(Debug, Default)]
pub struct LineInput {
    text: String,
    cursor: usize,
}

impl LineInput {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self { text, cursor }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Ширина текста до курсора в колонках терминала.
    pub fn cursor_width(&self) -> usize {
        self.text[..self.cursor].width()
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.text.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
    }

    pub fn delete(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.text.replace_range(self.cursor..next, "");
        }
    }

    pub fn left(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.cursor = prev;
        }
    }

    pub fn right(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.cursor = next;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Ctrl+W: удалить слово перед курсором вместе с пробелами за ним.
    pub fn delete_word(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end();
        let start = trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn prev_boundary(&self) -> Option<usize> {
        self.text[..self.cursor].char_indices().next_back().map(|(i, _)| i)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.text[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_multibyte() {
        let mut input = LineInput::new("кот");
        input.left();
        input.backspace();
        assert_eq!(input.text(), "кт");
        input.insert('и');
        assert_eq!(input.text(), "кит");
        assert_eq!(input.cursor_width(), 2);
        input.home();
        input.delete();
        assert_eq!(input.text(), "ит");
        input.backspace();
        assert_eq!(input.text(), "ит");
        input.end();
        input.delete();
        assert_eq!(input.cursor_width(), 2);
    }

    #[test]
    fn delete_word() {
        let mut input = LineInput::new("купить молоко  ");
        input.delete_word();
        assert_eq!(input.text(), "купить ");
        input.delete_word();
        assert_eq!(input.text(), "");
        input.delete_word();
        assert_eq!(input.text(), "");
    }
}
