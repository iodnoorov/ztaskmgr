//! Синхронизация через Syncthing. Если файл книжки поменяли на двух устройствах, пока они не
//! видели друг друга, Syncthing оставляет рядом копию проигравшей (более старой) версии:
//! `2026-10-26.sync-conflict-20261026-091500-ABCDEFG.md`. Здесь такие копии сливаются
//! с основным файлом и удаляются — страницы, записи, скрытые списки, архив удалённого.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::book::{AI_HIDDEN, Book, DELETED_NOTES, parse_hidden};
use crate::clock::Date;
use crate::model::{Board, Category, State, Task};
use crate::notes::Notes;
use crate::storage;

/// Временные файлы атомарной записи синхронизировать не нужно.
const STIGNORE: &str = "\
// ztaskmgr: временные файлы атомарной записи не синхронизировать
*.tmp
";

/// Имя основного файла по имени конфликтной копии: `2026-10-26.sync-conflict-….md` → `2026-10-26.md`.
pub fn conflict_base(name: &str) -> Option<String> {
    let (base, rest) = name.split_once(".sync-conflict-")?;
    let ext = rest.rsplit_once('.').map(|(_, ext)| ext)?;
    Some(format!("{base}.{ext}"))
}

/// Насколько задача «продвинулась»: при слиянии побеждает дальнейшее. Отменённая сильнее
/// выполненной (решение пользователя).
fn rank(state: State) -> u8 {
    match state {
        State::Open => 0,
        State::Migrated(_) => 1,
        State::Done => 2,
        State::Cancelled => 3,
    }
}

/// Где в `board` та же задача, что `task`: по id где угодно; иначе по тексту в том же списке —
/// среди задач без id или с id, которого нет у другой версии (одну и ту же написанную руками
/// задачу два устройства могли снабдить разными id).
fn find_same(board: &Board, task: &Task, category: Category, other_ids: &HashSet<&str>) -> Option<(Category, usize)> {
    if let Some(id) = &task.id {
        for c in Category::ALL {
            if let Some(i) = board.list(c).iter().position(|t| t.id.as_ref() == Some(id)) {
                return Some((c, i));
            }
        }
    }
    board
        .list(category)
        .iter()
        .position(|t| t.text == task.text && t.id.as_deref().is_none_or(|id| !other_ids.contains(id)))
        .map(|i| (category, i))
}

/// Слияние двух версий страницы. Основа — `current` (её текст, порядок и списки); задачи
/// из `other`, которых нет, встают после той, за которой стояли там (нет такой — в конец). У одной и той же
/// задачи — дальнейшее состояние; «скрыта» побеждает «открыта».
pub fn merge_boards(current: &Board, other: &Board) -> Board {
    let mut result = current.clone();
    let other_ids: HashSet<&str> = other.tasks().filter_map(|t| t.id.as_deref()).collect();
    for category in Category::ALL {
        let mut after: Option<usize> = None;
        for task in other.list(category) {
            match find_same(&result, task, category, &other_ids) {
                Some((c, i)) => {
                    let mine = result.get_mut(c, i).unwrap();
                    if rank(task.state) > rank(mine.state) {
                        mine.state = task.state;
                    }
                    mine.private |= task.private;
                    if mine.id.is_none() {
                        mine.id = task.id.clone();
                    }
                    after = (c == category).then_some(i);
                }
                None => {
                    // Без соседа выше — в конец: новые задачи и так добавляются в конец.
                    let at = after.map_or(result.list(category).len(), |i| i + 1);
                    result.insert(category, at, task.clone());
                    after = Some(at.min(result.list(category).len() - 1));
                }
            }
        }
    }
    result
}

/// Разделы архива удалённого: каждый начинается с `## `.
fn sections(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.starts_with("## ") || out.is_empty() {
            out.push(String::new());
        }
        let last = out.last_mut().unwrap();
        last.push_str(line);
        last.push('\n');
    }
    out
}

/// Что слилось при проверке.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Основные файлы, в которые слиты копии.
    pub merged: Vec<String>,
    /// Копии, которые слить не удалось (испорчены) — остаются на месте.
    pub failed: Vec<String>,
}

impl Book {
    /// Кладёт `.stignore` (или дописывает в него `*.tmp`), чтобы Syncthing не возил
    /// временные файлы. Каталог книжки создаётся, если его ещё нет.
    pub fn ensure_stignore(&self) -> Result<()> {
        fs::create_dir_all(self.dir()).with_context(|| format!("создание {}", self.dir().display()))?;
        let path = self.dir().join(".stignore");
        match fs::read_to_string(&path) {
            Ok(text) if text.lines().any(|l| l.trim() == "*.tmp") => Ok(()),
            Ok(text) => storage::write_atomic(&path, &format!("{}{}{STIGNORE}", text, if text.ends_with('\n') || text.is_empty() { "" } else { "\n" })),
            Err(_) => storage::write_atomic(&path, STIGNORE),
        }
    }

    /// Находит конфликтные копии Syncthing в книжке и сливает их с основными файлами.
    pub fn merge_conflicts(&self) -> Result<Report> {
        let mut report = Report::default();
        for (dir, notes) in [(self.dir().to_path_buf(), false), (self.dir().join("notes"), true)] {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok()?.file_name().into_string().ok())
                .filter(|n| n.contains(".sync-conflict-") && !n.ends_with(".tmp"))
                .collect();
            names.sort();
            for name in names {
                let Some(base) = conflict_base(&name) else { continue };
                let copy = dir.join(&name);
                match self.merge_one(&copy, &base, notes) {
                    Ok(true) => {
                        fs::remove_file(&copy).with_context(|| format!("удаление {}", copy.display()))?;
                        if !report.merged.contains(&base) {
                            report.merged.push(base);
                        }
                    }
                    Ok(false) => {}
                    Err(_) => report.failed.push(name),
                }
            }
        }
        Ok(report)
    }

    /// Сливает одну копию; `false` — файл не книжки, не трогаем.
    fn merge_one(&self, copy: &Path, base: &str, notes: bool) -> Result<bool> {
        let text = fs::read_to_string(copy)?;
        let stem = base.strip_suffix(".md");
        if notes {
            let Some(id) = stem.filter(|id| storage::is_valid_id(id)) else {
                return Ok(false);
            };
            let mut current = self.load_notes(id)?;
            current.merge(&Notes::parse(&text));
            self.save_notes(id, &current)?;
            return Ok(true);
        }
        if let Some(date) = stem.and_then(Date::parse_iso) {
            let other = storage::parse(&text)?;
            let current = self.load(date)?;
            self.save(date, &merge_boards(&current, &other))?;
            return Ok(true);
        }
        if base == AI_HIDDEN {
            // Скрыт хоть на одном устройстве — скрыт.
            let current = self.hidden()?;
            let other = parse_hidden(&text)?;
            self.set_hidden(std::array::from_fn(|i| current[i] || other[i]))?;
            return Ok(true);
        }
        if base == DELETED_NOTES {
            let path = self.dir().join(DELETED_NOTES);
            let mut current = fs::read_to_string(&path).unwrap_or_default();
            let have = sections(&current);
            for section in sections(&text) {
                if !have.contains(&section) {
                    current.push_str(&section);
                }
            }
            storage::write_atomic(&path, &current)?;
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(day: u32) -> Date {
        Date::new(2026, 10, day).unwrap()
    }

    fn task(text: &str, id: &str) -> Task {
        let mut t = Task::new(text);
        t.id = Some(id.into());
        t
    }

    fn texts(board: &Board, c: Category) -> Vec<String> {
        board.list(c).iter().map(|t| format!("{}{}", t.text, match t.state {
            State::Open => "",
            State::Done => " ✓",
            State::Cancelled => " ✗",
            State::Migrated(_) => " >",
        })).collect()
    }

    #[test]
    fn names() {
        assert_eq!(
            conflict_base("2026-10-26.sync-conflict-20261026-091500-ABCDEFG.md").as_deref(),
            Some("2026-10-26.md")
        );
        assert_eq!(conflict_base("2026-10-26.md"), None);
    }

    #[test]
    fn boards() {
        // Компьютер: отметил «a», добавил «c». Телефон: добавил «b» после «a», отметил «d».
        let mut pc = Board::default();
        let mut a = task("a", "1");
        a.state = State::Done;
        pc.push(Category::Work, a);
        pc.push(Category::Work, task("d", "4"));
        pc.push(Category::Work, task("c", "3"));
        let mut phone = Board::default();
        phone.push(Category::Work, task("a", "1"));
        phone.push(Category::Work, task("b", "2"));
        let mut d4 = task("d", "4");
        d4.state = State::Done;
        d4.private = true;
        phone.push(Category::Work, d4);
        let merged = merge_boards(&pc, &phone);
        assert_eq!(texts(&merged, Category::Work), ["a ✓", "b", "d ✓", "c"]);
        assert!(merged.list(Category::Work)[2].private);

        // Задачу перенесли в другой список на одном устройстве — не дублируется.
        let mut moved = Board::default();
        moved.push(Category::Home, task("a", "1"));
        let merged = merge_boards(&moved, &phone);
        assert_eq!(texts(&merged, Category::Home), ["a"]);
        assert_eq!(texts(&merged, Category::Work), ["b", "d ✓"]);

        // Написанная руками задача получила разные id на двух устройствах — одна задача.
        let mut one = Board::default();
        one.push(Category::Other, task("руками", "x1"));
        let mut two = Board::default();
        two.push(Category::Other, task("руками", "y2"));
        assert_eq!(texts(&merge_boards(&one, &two), Category::Other), ["руками"]);
    }

    #[test]
    fn cancelled_wins() {
        // Выполнили на одном устройстве, отменили на другом — отменена, в любом порядке.
        let mut done = Board::default();
        let mut t = task("a", "1");
        t.state = State::Done;
        done.push(Category::Work, t.clone());
        let mut cancelled = Board::default();
        t.state = State::Cancelled;
        cancelled.push(Category::Work, t);
        assert_eq!(texts(&merge_boards(&done, &cancelled), Category::Work), ["a ✗"]);
        assert_eq!(texts(&merge_boards(&cancelled, &done), Category::Work), ["a ✗"]);
    }

    #[test]
    fn merges_conflict_files() {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-sync-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let book = Book::new(dir.clone());
        book.ensure_stignore().unwrap();
        book.ensure_stignore().unwrap();
        assert_eq!(fs::read_to_string(dir.join(".stignore")).unwrap(), STIGNORE);

        let mut pc = Board::default();
        pc.push(Category::Work, task("с компьютера", "aa"));
        book.save(d(26), &pc).unwrap();
        let mut phone = Board::default();
        phone.push(Category::Work, task("с телефона", "bb"));
        fs::write(dir.join("2026-10-26.sync-conflict-20261026-091500-PHONE.md"), storage::serialize(&phone)).unwrap();

        let mut notes = Notes::default();
        notes.add_subtask("купить");
        book.save_notes("aa", &notes).unwrap();
        let mut other = Notes::default();
        other.add_subtask("купить");
        other.toggle(0);
        other.add_note("26.10.2026 09:00", "с телефона");
        fs::write(dir.join("notes/aa.sync-conflict-20261026-091500-PHONE.md"), other.serialize()).unwrap();

        fs::write(dir.join("2026-10-27.sync-conflict-20261026-091500-PHONE.md"), "испорчено\n").unwrap();
        fs::write(dir.join("photo.sync-conflict-20261026-091500-PHONE.jpg"), "").unwrap();

        let report = book.merge_conflicts().unwrap();
        assert_eq!(report.merged, ["2026-10-26.md", "aa.md"]);
        assert_eq!(report.failed, ["2026-10-27.sync-conflict-20261026-091500-PHONE.md"]);
        assert_eq!(texts(&book.load(d(26)).unwrap(), Category::Work), ["с компьютера", "с телефона"]);
        assert_eq!(book.load_notes("aa").unwrap().serialize(), "- [x] купить\n- 26.10.2026 09:00 с телефона\n");
        assert!(!dir.join("2026-10-26.sync-conflict-20261026-091500-PHONE.md").exists());
        // Чужие файлы не тронуты, испорченная копия осталась.
        assert!(dir.join("photo.sync-conflict-20261026-091500-PHONE.jpg").exists());
        assert!(dir.join("2026-10-27.sync-conflict-20261026-091500-PHONE.md").exists());
        assert_eq!(book.dates().unwrap(), [d(26)]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
