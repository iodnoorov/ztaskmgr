//! Слияние двух версий файла книжки — когда его поменяли и здесь, и на сервере WebDAV:
//! страницы, записи задач, реестр списков, архив удалённого.

use std::collections::HashSet;

use anyhow::{Context, Result};

use crate::book::{Book, FileKind, file_kind};
use crate::lists::Lists;
use crate::model::{Board, ListId, State, Task};
use crate::notes::Notes;
use crate::storage;

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
/// задачу два устройства могли снабдить разными id). По тексту сравниваются только задачи
/// без времени правки, то есть написанные руками: две задачи с одинаковым текстом, созданные
/// программой на двух устройствах, — разные задачи.
fn find_same(board: &Board, task: &Task, category: ListId, other_ids: &HashSet<&str>) -> Option<(ListId, usize)> {
    if let Some(id) = &task.id {
        for c in board.ids() {
            if let Some(i) = board.list(c).iter().position(|t| t.id.as_ref() == Some(id)) {
                return Some((c, i));
            }
        }
    }
    board
        .list(category)
        .iter()
        .position(|t| {
            task.modified == 0
                && t.modified == 0
                && t.text == task.text
                && t.id.as_deref().is_none_or(|id| !other_ids.contains(id))
        })
        .map(|i| (category, i))
}

/// Слияние двух версий страницы. Основа — `current` (её текст, порядок и списки); задачи
/// из `other`, которых нет, встают после той, за которой стояли там (нет такой — в конец).
///
/// У одной и той же задачи побеждает более поздняя правка (`Task::modified`), вместе с её
/// списком. Если время неизвестно или одинаково (файлы старых версий) — дальнейшее
/// состояние, «скрыта» побеждает «открыта». Удалённая задача (надгробие) не возвращается,
/// если её не меняли после удаления, а удалённая там — убирается и здесь.
pub fn merge_boards(current: &Board, other: &Board) -> Board {
    let mut result = current.clone();
    for (id, when) in other.graves() {
        result.bury(id, *when);
    }
    let other_ids: HashSet<&str> = other.tasks().filter_map(|t| t.id.as_deref()).collect();
    for category in other.ids() {
        if result.title(category).is_none()
            && let Some(title) = other.title(category)
        {
            result.set_title(category, title);
        }
        let mut after: Option<usize> = None;
        for task in other.list(category) {
            match find_same(&result, task, category, &other_ids) {
                Some((c, i)) => {
                    let mine = result.get_mut(c, i).unwrap();
                    let theirs_newer = task.modified > mine.modified;
                    if theirs_newer {
                        let id = mine.id.clone().or_else(|| task.id.clone());
                        *mine = task.clone();
                        mine.id = id;
                    } else if task.modified == mine.modified {
                        if rank(task.state) > rank(mine.state) {
                            mine.state = task.state;
                        }
                        mine.private |= task.private;
                    }
                    if mine.id.is_none() {
                        mine.id = task.id.clone();
                    }
                    if theirs_newer && c != category {
                        // Там её переложили в другой список позже — туда же и здесь.
                        let moved = result.remove(c, i).unwrap();
                        let at = after.map_or(result.list(category).len(), |i| i + 1);
                        result.insert(category, at, moved);
                        after = Some(at.min(result.list(category).len() - 1));
                    } else {
                        after = (c == category).then_some(i);
                    }
                }
                None => {
                    // Удалили здесь, а там с тех пор не меняли — не возвращаем.
                    let buried = task.id.as_deref().and_then(|id| result.buried(id));
                    if buried.is_some_and(|when| when >= task.modified) {
                        continue;
                    }
                    // Без соседа выше — в конец: новые задачи и так добавляются в конец.
                    let at = after.map_or(result.list(category).len(), |i| i + 1);
                    result.insert(category, at, task.clone());
                    after = Some(at.min(result.list(category).len() - 1));
                }
            }
        }
    }
    // Удалили там, а здесь с тех пор не меняли — убираем и здесь.
    for category in result.ids() {
        let doomed: Vec<usize> = result
            .list(category)
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                let buried = t.id.as_deref().and_then(|id| other.buried(id));
                buried.is_some_and(|when| when >= t.modified)
            })
            .map(|(i, _)| i)
            .collect();
        for i in doomed.into_iter().rev() {
            result.remove(category, i);
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

/// Слияние двух версий файла книжки `path`: наша (`ours`, нет файла — `None`) и другая
/// (`theirs`). Чистая функция: ничего не читает и не пишет. Результат — текст файла (пустой —
/// файла быть не должно); `None` — это не файл книжки.
pub fn merge_text(path: &str, ours: Option<&str>, theirs: &str, lists: &Lists) -> Result<Option<String>> {
    let ours = ours.unwrap_or("");
    let text = match file_kind(path) {
        None => return Ok(None),
        Some(FileKind::Notes(_)) => {
            let mut notes = Notes::parse(ours);
            notes.merge(&Notes::parse(theirs));
            if notes.is_empty() { String::new() } else { notes.serialize() }
        }
        Some(FileKind::Page(_)) => {
            let mine = storage::parse(ours, lists)?;
            let other = storage::parse(theirs, lists)?;
            storage::serialize(&merge_boards(&mine, &other), lists)
        }
        Some(FileKind::Lists) => {
            let mut mine = if ours.trim().is_empty() { Lists::default() } else { Lists::parse(ours)? };
            mine.merge(&Lists::parse(theirs)?);
            mine.serialize()
        }
        Some(FileKind::Deleted) => {
            let mut text = ours.to_string();
            let have = sections(ours);
            for section in sections(theirs) {
                if !have.contains(&section) {
                    text.push_str(&section);
                }
            }
            text
        }
    };
    Ok(Some(text))
}

impl Book {
    /// Сливает другую версию файла книжки (`text`) с нашим файлом `path` и сохраняет
    /// результат. `false` — файл не книжки, не трогаем.
    pub fn merge_text(&self, path: &str, text: &str) -> Result<bool> {
        let ours = self.read_file(path)?.map(String::from_utf8).transpose().context("не текст в UTF-8")?;
        let Some(merged) = merge_text(path, ours.as_deref(), text, &self.lists()?)? else {
            return Ok(false);
        };
        if merged.is_empty() {
            self.remove_file(path)?;
        } else {
            self.write_file(path, merged.as_bytes())?;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::clock::Date;

    fn d(day: u32) -> Date {
        Date::new(2026, 10, day).unwrap()
    }

    fn task(text: &str, id: &str) -> Task {
        let mut t = Task::new(text);
        t.id = Some(id.into());
        t
    }

    fn texts(board: &Board, c: ListId) -> Vec<String> {
        board
            .list(c)
            .iter()
            .map(|t| {
                format!(
                    "{}{}",
                    t.text,
                    match t.state {
                        State::Open => "",
                        State::Done => " ✓",
                        State::Cancelled => " ✗",
                        State::Migrated(_) => " >",
                    }
                )
            })
            .collect()
    }

    #[test]
    fn boards() {
        // Компьютер: отметил «a», добавил «c». Телефон: добавил «b» после «a», отметил «d».
        let mut pc = Board::default();
        let mut a = task("a", "1");
        a.state = State::Done;
        pc.push(ListId::WORK, a);
        pc.push(ListId::WORK, task("d", "4"));
        pc.push(ListId::WORK, task("c", "3"));
        let mut phone = Board::default();
        phone.push(ListId::WORK, task("a", "1"));
        phone.push(ListId::WORK, task("b", "2"));
        let mut d4 = task("d", "4");
        d4.state = State::Done;
        d4.private = true;
        phone.push(ListId::WORK, d4);
        let merged = merge_boards(&pc, &phone);
        assert_eq!(texts(&merged, ListId::WORK), ["a ✓", "b", "d ✓", "c"]);
        assert!(merged.list(ListId::WORK)[2].private);

        // Задачу перенесли в другой список на одном устройстве — не дублируется.
        let mut moved = Board::default();
        moved.push(ListId::HOME, task("a", "1"));
        let merged = merge_boards(&moved, &phone);
        assert_eq!(texts(&merged, ListId::HOME), ["a"]);
        assert_eq!(texts(&merged, ListId::WORK), ["b", "d ✓"]);

        // Написанная руками задача получила разные id на двух устройствах — одна задача.
        let mut one = Board::default();
        one.push(ListId::OTHER, task("руками", "x1"));
        let mut two = Board::default();
        two.push(ListId::OTHER, task("руками", "y2"));
        assert_eq!(texts(&merge_boards(&one, &two), ListId::OTHER), ["руками"]);
    }

    #[test]
    fn merges_lists_file() {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-merge-lists-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let book = Book::new(dir.clone());
        let mut pc = Lists::default();
        pc.add(crate::model::ListId::new("pc1"), "С компьютера").unwrap();
        book.save_lists(&pc).unwrap();
        let mut phone = Lists::default();
        phone.add(crate::model::ListId::new("ph1"), "С телефона").unwrap();
        assert!(book.merge_text("LISTS.md", &phone.serialize()).unwrap());
        let titles: Vec<String> = book.lists().unwrap().all().iter().map(|d| d.title.clone()).collect();
        assert_eq!(titles, ["Работа", "Дом", "Личное", "Прочее", "С телефона", "С компьютера"]);
        let _ = fs::remove_dir_all(&dir);
    }

    fn at(text: &str, id: &str, state: State, modified: u64) -> Task {
        let mut t = task(text, id);
        t.state = state;
        t.modified = modified;
        t
    }

    #[test]
    fn later_edit_wins() {
        // Отметку сняли позже, чем поставили на другом устройстве, — открыта, в любом порядке.
        let mut a = Board::default();
        a.push(ListId::WORK, at("x", "1", State::Done, 100));
        let mut b = Board::default();
        b.push(ListId::WORK, at("x правка", "1", State::Open, 200));
        assert_eq!(texts(&merge_boards(&a, &b), ListId::WORK), ["x правка"]);
        assert_eq!(texts(&merge_boards(&b, &a), ListId::WORK), ["x правка"]);
        // Позже переложили в другой список — туда же.
        let mut c = Board::default();
        c.push(ListId::HOME, at("x", "1", State::Open, 300));
        let merged = merge_boards(&a, &c);
        assert!(merged.list(ListId::WORK).is_empty());
        assert_eq!(texts(&merged, ListId::HOME), ["x"]);
    }

    #[test]
    fn same_text_different_tasks() {
        // Две задачи «Купить хлеб», созданные программой на двух устройствах, — две задачи.
        let mut a = Board::default();
        a.push(ListId::HOME, at("Купить хлеб", "1", State::Open, 100));
        let mut b = Board::default();
        b.push(ListId::HOME, at("Купить хлеб", "2", State::Open, 200));
        assert_eq!(merge_boards(&a, &b).list(ListId::HOME).len(), 2);
        // Написанную руками (без времени) два устройства снабдили разными id — одна задача.
        let mut c = Board::default();
        c.push(ListId::HOME, task("руками", "x1"));
        let mut d = Board::default();
        d.push(ListId::HOME, task("руками", "y2"));
        assert_eq!(merge_boards(&c, &d).list(ListId::HOME).len(), 1);
    }

    #[test]
    fn graves() {
        let mut deleted = Board::default();
        deleted.push(ListId::WORK, at("остаётся", "2", State::Open, 50));
        deleted.bury("1", 300);
        // Там задачу с тех пор не меняли — не воскресает, в любом порядке.
        let mut old = Board::default();
        old.push(ListId::WORK, at("удалённая", "1", State::Open, 200));
        old.push(ListId::WORK, at("остаётся", "2", State::Open, 50));
        assert_eq!(texts(&merge_boards(&deleted, &old), ListId::WORK), ["остаётся"]);
        let merged = merge_boards(&old, &deleted);
        assert_eq!(texts(&merged, ListId::WORK), ["остаётся"]);
        assert_eq!(merged.buried("1"), Some(300));
        // Там её поправили после удаления — правка побеждает.
        let mut edited = Board::default();
        edited.push(ListId::WORK, at("поправили", "1", State::Open, 400));
        assert_eq!(texts(&merge_boards(&deleted, &edited), ListId::WORK), ["остаётся", "поправили"]);
        assert_eq!(texts(&merge_boards(&edited, &deleted), ListId::WORK), ["поправили", "остаётся"]);
    }

    #[test]
    fn cancelled_wins() {
        // Выполнили на одном устройстве, отменили на другом — отменена, в любом порядке.
        let mut done = Board::default();
        let mut t = task("a", "1");
        t.state = State::Done;
        done.push(ListId::WORK, t.clone());
        let mut cancelled = Board::default();
        t.state = State::Cancelled;
        cancelled.push(ListId::WORK, t);
        assert_eq!(texts(&merge_boards(&done, &cancelled), ListId::WORK), ["a ✗"]);
        assert_eq!(texts(&merge_boards(&cancelled, &done), ListId::WORK), ["a ✗"]);
    }

    #[test]
    fn merges_files() {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-merge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let book = Book::new(dir.clone());

        let mut pc = Board::default();
        pc.push(ListId::WORK, task("с компьютера", "aa"));
        book.save(d(26), &pc).unwrap();
        let mut phone = Board::default();
        phone.push(ListId::WORK, task("с телефона", "bb"));
        let text = storage::serialize(&phone, &Lists::default());
        assert!(book.merge_text("2026-10-26.md", &text).unwrap());
        assert_eq!(texts(&book.load(d(26)).unwrap(), ListId::WORK), ["с компьютера", "с телефона"]);

        let mut notes = Notes::default();
        notes.add_subtask("купить");
        book.save_notes("aa", &notes).unwrap();
        let mut other = Notes::default();
        other.add_subtask("купить");
        other.toggle(0);
        other.add_note("26.10.2026 09:00", "с телефона");
        assert!(book.merge_text("notes/aa.md", &other.serialize()).unwrap());
        assert_eq!(book.load_notes("aa").unwrap().serialize(), "- [x] купить\n- 26.10.2026 09:00 с телефона\n");

        // Испорченная версия — ошибка, файл не тронут; чужие файлы не сливаются.
        assert!(book.merge_text("2026-10-26.md", "испорчено\n").is_err());
        assert_eq!(texts(&book.load(d(26)).unwrap(), ListId::WORK), ["с компьютера", "с телефона"]);
        assert!(!book.merge_text("photo.jpg", "").unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }
}
