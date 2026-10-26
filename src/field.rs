use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// Многострочное поле: текст с `\n`, перенос по ширине поля (посимвольно, чтобы курсор
/// стоял точно), курсор — байтовый индекс на границе символа. Ширину сообщает отрисовка
/// (`set_width`), по ней работают ↑ ↓ и Home / End — по экранным строкам.
#[derive(Debug)]
pub struct TextArea {
    text: String,
    cursor: usize,
    width: usize,
}

impl TextArea {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self {
            text,
            cursor,
            width: 60,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_width(&mut self, width: usize) {
        self.width = width.max(2);
    }

    /// Экранные строки: байтовые диапазоны. Последняя колонка остаётся под курсор.
    pub fn rows(&self) -> Vec<(usize, usize)> {
        let limit = self.width - 1;
        let mut rows = Vec::new();
        let mut start = 0;
        for line in self.text.split('\n') {
            let mut row_start = start;
            let mut used = 0;
            for (i, c) in line.char_indices() {
                let w = c.width().unwrap_or(0);
                if used + w > limit && used > 0 {
                    rows.push((row_start, start + i));
                    row_start = start + i;
                    used = 0;
                }
                used += w;
            }
            rows.push((row_start, start + line.len()));
            start += line.len() + 1;
        }
        rows
    }

    /// Курсор: номер экранной строки и колонка в ней.
    pub fn cursor_pos(&self) -> (usize, usize) {
        let rows = self.rows();
        for (r, &(start, end)) in rows.iter().enumerate() {
            // Курсор на стыке перенесённых строк — в начале следующей.
            let last_in_line = rows.get(r + 1).is_none_or(|&(next, _)| next != end);
            if self.cursor >= start && (self.cursor < end || (self.cursor == end && last_in_line)) {
                return (r, self.text[start..self.cursor].width());
            }
        }
        (rows.len() - 1, 0)
    }

    /// Байт в строке `row`, ближайший к колонке `col` слева.
    fn at_column(&self, (start, end): (usize, usize), col: usize) -> usize {
        let mut used = 0;
        for (i, c) in self.text[start..end].char_indices() {
            used += c.width().unwrap_or(0);
            if used > col {
                return start + i;
            }
        }
        end
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn newline(&mut self) {
        self.insert('\n');
    }

    pub fn backspace(&mut self) {
        if let Some((i, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.text.replace_range(i..self.cursor, "");
            self.cursor = i;
        }
    }

    pub fn delete(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.text.replace_range(self.cursor..self.cursor + c.len_utf8(), "");
        }
    }

    pub fn left(&mut self) {
        if let Some((i, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = i;
        }
    }

    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    pub fn up(&mut self) {
        let (row, col) = self.cursor_pos();
        if row > 0 {
            self.cursor = self.at_column(self.rows()[row - 1], col);
        }
    }

    pub fn down(&mut self) {
        let (row, col) = self.cursor_pos();
        if let Some(&next) = self.rows().get(row + 1) {
            self.cursor = self.at_column(next, col);
        }
    }

    /// В начало экранной строки.
    pub fn home(&mut self) {
        let (row, _) = self.cursor_pos();
        self.cursor = self.rows()[row].0;
    }

    /// В конец экранной строки.
    pub fn end(&mut self) {
        let (row, _) = self.cursor_pos();
        self.cursor = self.rows()[row].1;
    }

    /// Ctrl+W: удалить слово перед курсором вместе с пробелами за ним (не дальше начала строки).
    pub fn delete_word(&mut self) {
        let line_start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let before = &self.text[line_start..self.cursor];
        let trimmed = before.trim_end();
        let start = line_start
            + trimmed
                .char_indices()
                .rev()
                .find(|(_, c)| c.is_whitespace())
                .map_or(0, |(i, c)| i + c.len_utf8());
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
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
    fn text_area_rows_and_cursor() {
        let mut area = TextArea::new("");
        area.set_width(6);
        for c in "абвгдеж".chars() {
            area.insert(c);
        }
        // Ширина 6, под текст 5 колонок: «абвгд» + «еж».
        assert_eq!(area.rows().len(), 2);
        assert_eq!(area.cursor_pos(), (1, 2));
        area.newline();
        area.insert('з');
        assert_eq!(area.text(), "абвгдеж\nз");
        assert_eq!(area.cursor_pos(), (2, 1));
        area.up();
        assert_eq!(area.cursor_pos(), (1, 1));
        area.up();
        assert_eq!(area.cursor_pos(), (0, 1));
        area.home();
        assert_eq!(area.cursor_pos(), (0, 0));
        area.end();
        // Конец перенесённой строки — начало следующей.
        assert_eq!(area.cursor_pos(), (1, 0));
        area.down();
        area.down();
        assert_eq!(area.cursor_pos(), (2, 0));
        area.backspace();
        assert_eq!(area.text(), "абвгдежз");
        // Пустые строки — тоже строки.
        let mut area = TextArea::new("a\n\nb");
        area.up();
        assert_eq!(area.cursor_pos(), (1, 0));
        area.insert('x');
        assert_eq!(area.text(), "a\nx\nb");
    }

    #[test]
    fn text_area_delete_word_stops_at_line() {
        let mut area = TextArea::new("первая строка\nвторая");
        area.delete_word();
        assert_eq!(area.text(), "первая строка\n");
        area.delete_word();
        assert_eq!(area.text(), "первая строка\n");
        area.backspace();
        area.delete_word();
        assert_eq!(area.text(), "первая ");
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
