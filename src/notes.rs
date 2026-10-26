//! Доп. записи по задаче: Markdown-список в `notes/<id>.md`. Заметка — `- 2026-10-26 19:40 текст`,
//! её следующие строки — с отступом в два пробела (пустые строки внутри заметки — пустые);
//! подзадача — `- [ ] текст`. Файл правится и во внешнем редакторе, поэтому любые другие строки
//! сохраняются как есть, а пустые между пунктами — на своих местах.

/// Отступ строк продолжения пункта.
const INDENT: &str = "  ";

/// Пункт записей: первая строка и строки продолжения (как в файле, с отступом), либо пустая
/// строка между пунктами (`lines == [""]`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    lines: Vec<String>,
}

impl Entry {
    fn is_blank(&self) -> bool {
        self.lines[0].is_empty()
    }

    fn head(&self) -> &str {
        &self.lines[0]
    }

    /// Строки продолжения без отступа.
    fn body(&self) -> impl Iterator<Item = &str> {
        self.lines[1..]
            .iter()
            .map(|l| l.strip_prefix(INDENT).unwrap_or(l.trim_start()))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notes {
    entries: Vec<Entry>,
}

/// Вид пункта.
#[derive(Debug, PartialEq, Eq)]
pub enum Kind<'a> {
    Subtask { done: bool },
    Note { stamp: &'a str },
    Text,
}

/// Пункт для показа: вид, текст первой строки и строки продолжения.
#[derive(Debug, PartialEq, Eq)]
pub struct Item<'a> {
    pub kind: Kind<'a>,
    pub head: &'a str,
    pub body: Vec<&'a str>,
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

/// Вид и текст первой строки пункта.
fn head_kind(line: &str) -> (Kind<'_>, &str) {
    if let Some(text) = line.strip_prefix("- [ ] ") {
        return (Kind::Subtask { done: false }, text);
    }
    if let Some(text) = line
        .strip_prefix("- [x] ")
        .or_else(|| line.strip_prefix("- [X] "))
    {
        return (Kind::Subtask { done: true }, text);
    }
    if let Some(rest) = line.strip_prefix("- ")
        && rest.len() >= 16
        && rest.is_char_boundary(16)
        && is_stamp(&rest[..16])
    {
        return (Kind::Note { stamp: &rest[..16] }, rest[16..].trim_start());
    }
    (Kind::Text, line)
}

/// Строки текста заметки — в строки файла: первая — после метки, остальные — с отступом.
fn note_lines(head: String, text: &str) -> Vec<String> {
    let mut parts = text.split('\n');
    let mut lines = vec![format!("{head}{}", parts.next().unwrap_or(""))];
    lines.extend(parts.map(|l| {
        if l.is_empty() {
            String::new()
        } else {
            format!("{INDENT}{l}")
        }
    }));
    lines
}

impl Notes {
    pub fn parse(text: &str) -> Self {
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        let mut entries: Vec<Entry> = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            let continues = line.starts_with(' ') || line.starts_with('\t');
            if continues && let Some(last) = entries.last_mut().filter(|e| !e.is_blank()) {
                last.lines.push(line.to_string());
            } else if line.is_empty() {
                // Пустые строки, за которыми идёт продолжение, — часть пункта (как в Markdown).
                let next = lines[i..].iter().position(|l| !l.is_empty()).map(|n| i + n);
                let inside = next.is_some_and(|n| lines[n].starts_with(INDENT))
                    && entries.last().is_some_and(|e| !e.is_blank());
                match (inside, next) {
                    (true, Some(n)) => {
                        let last = entries.last_mut().unwrap();
                        last.lines.extend((i..n).map(|_| String::new()));
                        i = n;
                        continue;
                    }
                    _ => entries.push(Entry { lines: vec![String::new()] }),
                }
            } else {
                entries.push(Entry { lines: vec![line.to_string()] });
            }
            i += 1;
        }
        while entries.last().is_some_and(Entry::is_blank) {
            entries.pop();
        }
        Self { entries }
    }

    pub fn serialize(&self) -> String {
        self.entries
            .iter()
            .flat_map(|e| &e.lines)
            .map(|l| format!("{l}\n"))
            .collect()
    }

    /// Без единого пункта — записей нет, файл не нужен.
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(Entry::is_blank)
    }

    /// Номера пунктов (не пустых строк между ними) — то, что показывает окно записей.
    pub fn visible(&self) -> Vec<usize> {
        (0..self.entries.len())
            .filter(|&i| !self.entries[i].is_blank())
            .collect()
    }

    pub fn item(&self, i: usize) -> Item<'_> {
        let entry = &self.entries[i];
        let (kind, head) = head_kind(entry.head());
        Item {
            kind,
            head,
            body: entry.body().collect(),
        }
    }

    pub fn summary(&self) -> Summary {
        let mut summary = Summary {
            any: !self.is_empty(),
            ..Summary::default()
        };
        for entry in &self.entries {
            if let (Kind::Subtask { done }, _) = head_kind(entry.head()) {
                summary.subtasks += 1;
                summary.subtasks_done += usize::from(done);
            }
        }
        summary
    }

    /// Заметка, текст может быть в несколько строк. Возвращает номер пункта.
    pub fn add_note(&mut self, stamp: &str, text: &str) -> usize {
        self.entries.push(Entry {
            lines: note_lines(format!("- {stamp} "), text),
        });
        self.entries.len() - 1
    }

    pub fn add_subtask(&mut self, text: &str) {
        self.entries.push(Entry {
            lines: vec![format!("- [ ] {text}")],
        });
    }

    /// Можно ли править пункт многострочно: заметки и прочий текст — да, подзадачи — нет.
    pub fn is_multiline(&self, i: usize) -> bool {
        !matches!(self.item(i).kind, Kind::Subtask { .. })
    }

    /// Текст пункта без разметки — то, что правится в поле ввода. У подзадачи — только первая
    /// строка (продолжение, если его дописали в редакторе, при правке остаётся как есть).
    pub fn editable_text(&self, i: usize) -> String {
        let item = self.item(i);
        if let Kind::Subtask { .. } = item.kind {
            return item.head.to_string();
        }
        std::iter::once(item.head)
            .chain(item.body)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Новый текст пункта; отметка подзадачи и метка заметки остаются.
    pub fn set_text(&mut self, i: usize, text: &str) {
        let entry = &mut self.entries[i];
        match head_kind(entry.head()) {
            (Kind::Subtask { done }, _) => {
                entry.lines[0] = format!("- [{}] {text}", if done { 'x' } else { ' ' });
            }
            (Kind::Note { stamp }, _) => entry.lines = note_lines(format!("- {stamp} "), text),
            (Kind::Text, _) => entry.lines = note_lines(String::new(), text),
        }
    }

    /// Отметка подзадачи; у других пунктов — ничего. Возвращает, была ли это подзадача.
    pub fn toggle(&mut self, i: usize) -> bool {
        let entry = &mut self.entries[i];
        let (Kind::Subtask { done }, text) = head_kind(entry.head()) else {
            return false;
        };
        entry.lines[0] = format!("- [{}] {text}", if done { ' ' } else { 'x' });
        true
    }

    /// Сливает записи с другого устройства: пункты, которых здесь нет, дописываются в конец;
    /// подзадача, отмеченная хоть на одном устройстве, отмечена.
    pub fn merge(&mut self, other: &Notes) {
        for entry in other.entries.iter().filter(|e| !e.is_blank()) {
            if self.entries.contains(entry) {
                continue;
            }
            let (kind, head) = head_kind(entry.head());
            let same_subtask = self.entries.iter().position(|e| {
                matches!(head_kind(e.head()), (Kind::Subtask { .. }, h) if h == head)
            });
            match (kind, same_subtask) {
                (Kind::Subtask { done }, Some(i)) => {
                    if done && !matches!(head_kind(self.entries[i].head()).0, Kind::Subtask { done: true }) {
                        self.toggle(i);
                    }
                }
                _ => self.entries.push(entry.clone()),
            }
        }
    }

    /// Пункт как в файле — для архива удалённого.
    pub fn raw(&self, i: usize) -> String {
        self.entries[i].lines.iter().map(|l| format!("{l}\n")).collect()
    }

    pub fn remove(&mut self, i: usize) {
        self.entries.remove(i);
    }

    /// Меняет местами два пункта (соседние видимые в окне).
    pub fn swap(&mut self, a: usize, b: usize) {
        self.entries.swap(a, b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
- [ ] купить прокладку
- [x] перекрыть воду

- 2026-10-26 19:40 мастер сказал:
  менять смеситель целиком

  прокладку можно не покупать
Просто абзац из редактора
";

    #[test]
    fn items_and_summary() {
        let notes = Notes::parse(SAMPLE);
        assert_eq!(notes.visible(), [0, 1, 3, 4]);
        assert_eq!(notes.item(0).kind, Kind::Subtask { done: false });
        assert_eq!(notes.item(0).head, "купить прокладку");
        let note = notes.item(3);
        assert_eq!(note.kind, Kind::Note { stamp: "2026-10-26 19:40" });
        assert_eq!(note.head, "мастер сказал:");
        assert_eq!(note.body, ["менять смеситель целиком", "", "прокладку можно не покупать"]);
        assert_eq!(notes.item(4).kind, Kind::Text);
        assert_eq!(head_kind("- 2026-10-2x 19:40 нет").0, Kind::Text);
        assert_eq!(head_kind("- коротко").0, Kind::Text);
        assert_eq!(
            notes.summary(),
            Summary { any: true, subtasks_done: 1, subtasks: 2 }
        );
        assert_eq!(notes.serialize(), SAMPLE);
        assert_eq!(Notes::parse("\n\n").summary(), Summary::default());
        // Отступ в начале файла — не продолжение, а обычный текст.
        assert_eq!(Notes::parse("  сам по себе\n").visible(), [0]);
    }

    #[test]
    fn multiline_note_round_trip() {
        let mut notes = Notes::default();
        notes.add_note("2026-10-26 19:40", "первая\n\nтретья");
        assert_eq!(notes.serialize(), "- 2026-10-26 19:40 первая\n\n  третья\n");
        assert_eq!(Notes::parse(&notes.serialize()), notes);
        assert!(notes.is_multiline(0));
        assert_eq!(notes.editable_text(0), "первая\n\nтретья");
        notes.set_text(0, "одна строка");
        assert_eq!(notes.serialize(), "- 2026-10-26 19:40 одна строка\n");
    }

    #[test]
    fn edit_entries() {
        let mut notes = Notes::default();
        assert!(notes.is_empty());
        notes.add_subtask("купить");
        notes.add_note("2026-10-26 19:40", "позвонил");
        assert!(notes.toggle(0));
        assert!(!notes.toggle(1));
        assert!(!notes.is_multiline(0));
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
        // Продолжение подзадачи из редактора при правке первой строки остаётся.
        let mut notes = Notes::parse("- [ ] пункт\n  подробности\n");
        notes.set_text(0, "пункт поправлен");
        assert_eq!(notes.serialize(), "- [ ] пункт поправлен\n  подробности\n");
    }
}
