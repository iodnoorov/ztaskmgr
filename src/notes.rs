//! Доп. записи по задаче: Markdown-список в `notes/<id>.md`. Заметка — `- 2026-10-26 19:40 текст`,
//! подзадача — `- [ ] текст`. Файл правится и во внешнем редакторе, поэтому любые другие строки
//! сохраняются как есть, а пустые — на своих местах.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notes {
    lines: Vec<String>,
}

/// Как показывать строку записей.
#[derive(Debug, PartialEq, Eq)]
pub enum Item<'a> {
    Subtask { done: bool, text: &'a str },
    Note { stamp: &'a str, text: &'a str },
    Text(&'a str),
}

/// Что видно на странице у задачи: есть ли записи и сколько подзадач выполнено.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub any: bool,
    pub subtasks_done: usize,
    pub subtasks: usize,
}

/// `2026-10-26 19:40` — метка заметки.
fn is_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 16
        && b.iter().enumerate().all(|(i, &c)| match i {
            4 | 7 => c == b'-',
            10 => c == b' ',
            13 => c == b':',
            _ => c.is_ascii_digit(),
        })
}

pub fn item(line: &str) -> Item<'_> {
    if let Some(text) = line.strip_prefix("- [ ] ") {
        return Item::Subtask { done: false, text };
    }
    if let Some(text) = line
        .strip_prefix("- [x] ")
        .or_else(|| line.strip_prefix("- [X] "))
    {
        return Item::Subtask { done: true, text };
    }
    if let Some(rest) = line.strip_prefix("- ")
        && rest.len() >= 16
        && rest.is_char_boundary(16)
        && is_stamp(&rest[..16])
    {
        return Item::Note {
            stamp: &rest[..16],
            text: rest[16..].trim_start(),
        };
    }
    Item::Text(line)
}

impl Notes {
    pub fn parse(text: &str) -> Self {
        let mut lines: Vec<String> = text.lines().map(|l| l.trim_end().to_string()).collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        Self { lines }
    }

    pub fn serialize(&self) -> String {
        self.lines.iter().map(|l| format!("{l}\n")).collect()
    }

    /// Без единой непустой строки — записей нет, файл не нужен.
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.is_empty())
    }

    /// Номера непустых строк — то, что показывает окно записей.
    pub fn visible(&self) -> Vec<usize> {
        (0..self.lines.len())
            .filter(|&i| !self.lines[i].is_empty())
            .collect()
    }

    pub fn line(&self, i: usize) -> &str {
        &self.lines[i]
    }

    pub fn summary(&self) -> Summary {
        let mut summary = Summary {
            any: !self.is_empty(),
            ..Summary::default()
        };
        for line in &self.lines {
            if let Item::Subtask { done, .. } = item(line) {
                summary.subtasks += 1;
                summary.subtasks_done += usize::from(done);
            }
        }
        summary
    }

    pub fn add_note(&mut self, stamp: &str, text: &str) {
        self.lines.push(format!("- {stamp} {text}"));
    }

    pub fn add_subtask(&mut self, text: &str) {
        self.lines.push(format!("- [ ] {text}"));
    }

    /// Текст строки без разметки — то, что правится в поле ввода.
    pub fn editable_text(&self, i: usize) -> &str {
        match item(&self.lines[i]) {
            Item::Subtask { text, .. } | Item::Note { text, .. } | Item::Text(text) => text,
        }
    }

    /// Новый текст строки; отметка подзадачи и метка заметки остаются.
    pub fn set_text(&mut self, i: usize, text: &str) {
        self.lines[i] = match item(&self.lines[i]) {
            Item::Subtask { done, .. } => format!("- [{}] {text}", if done { 'x' } else { ' ' }),
            Item::Note { stamp, .. } => format!("- {stamp} {text}"),
            Item::Text(_) => text.to_string(),
        };
    }

    /// Отметка подзадачи; у других строк — ничего. Возвращает, была ли это подзадача.
    pub fn toggle(&mut self, i: usize) -> bool {
        let Item::Subtask { done, text } = item(&self.lines[i]) else {
            return false;
        };
        self.lines[i] = format!("- [{}] {text}", if done { ' ' } else { 'x' });
        true
    }

    pub fn remove(&mut self, i: usize) {
        self.lines.remove(i);
    }

    /// Меняет местами две строки (соседние видимые в окне).
    pub fn swap(&mut self, a: usize, b: usize) {
        self.lines.swap(a, b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
- [ ] купить прокладку
- [x] перекрыть воду

- 2026-10-26 19:40 мастер сказал — менять смеситель
Просто абзац из редактора
";

    #[test]
    fn items_and_summary() {
        let notes = Notes::parse(SAMPLE);
        assert_eq!(notes.visible(), [0, 1, 3, 4]);
        assert_eq!(
            item(notes.line(0)),
            Item::Subtask { done: false, text: "купить прокладку" }
        );
        assert_eq!(
            item(notes.line(3)),
            Item::Note { stamp: "2026-10-26 19:40", text: "мастер сказал — менять смеситель" }
        );
        assert_eq!(item(notes.line(4)), Item::Text("Просто абзац из редактора"));
        assert_eq!(item("- 2026-10-2x 19:40 нет"), Item::Text("- 2026-10-2x 19:40 нет"));
        assert_eq!(item("- коротко"), Item::Text("- коротко"));
        assert_eq!(
            notes.summary(),
            Summary { any: true, subtasks_done: 1, subtasks: 2 }
        );
        assert_eq!(notes.serialize(), SAMPLE);
        assert_eq!(Notes::parse("\n\n").summary(), Summary::default());
    }

    #[test]
    fn edit_lines() {
        let mut notes = Notes::default();
        assert!(notes.is_empty());
        notes.add_subtask("купить");
        notes.add_note("2026-10-26 19:40", "позвонил");
        assert!(notes.toggle(0));
        assert!(!notes.toggle(1));
        assert_eq!(notes.editable_text(0), "купить");
        notes.set_text(0, "купить две");
        notes.set_text(1, "позвонил ещё раз");
        assert_eq!(
            notes.serialize(),
            "- [x] купить две\n- 2026-10-26 19:40 позвонил ещё раз\n"
        );
        notes.swap(0, 1);
        notes.remove(1);
        assert_eq!(notes.serialize(), "- 2026-10-26 19:40 позвонил ещё раз\n");
    }
}
