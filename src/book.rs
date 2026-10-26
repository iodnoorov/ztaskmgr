//! Книжка: каталог со страницами по датам, `2026-10-26.md` — четыре списка на этот день.

use std::collections::HashMap;
use std::fs;
use std::hash::{BuildHasher, RandomState};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::os::unix::fs::MetadataExt;
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::clock::Date;
use crate::model::{Board, Category, State, Task};
use crate::notes::{Notes, Summary};
use crate::storage;

/// Куда уходят удалённые записи, чтобы не терять их.
pub const DELETED_NOTES: &str = "DELETED_NOTES.md";
/// Какие списки скрыты от ИИ: `- Личное` по строке на список.
pub const AI_HIDDEN: &str = "AI_HIDDEN.md";

/// Скрытые списки из текста `AI_HIDDEN.md`: `- Личное` по строке на список.
pub fn parse_hidden(text: &str) -> Result<[bool; 4]> {
    let mut hidden = [false; 4];
    for line in text.lines().filter_map(|l| l.trim().strip_prefix("- ")) {
        let category = Category::from_title(line.trim())
            .with_context(|| format!("неизвестный список «{line}»"))?;
        hidden[category.index()] = true;
    }
    Ok(hidden)
}

/// Отпечаток файла: время изменения, размер и inode. По нему видно правки снаружи (агент,
/// синхронизация, редактор). Время в Linux грубое (миллисекунды), две записи подряд могут
/// его не сдвинуть, поэтому ещё размер и inode: запись через rename всегда даёт новый inode.
pub type Stamp = (SystemTime, u64, u64);

/// Нет файла — `None`.
fn stamp(path: &Path) -> Option<Stamp> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len(), meta.ino()))
}

pub struct Book {
    dir: PathBuf,
}

impl Book {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn page_path(&self, date: Date) -> PathBuf {
        self.dir.join(format!("{date}.md"))
    }

    /// Страница на дату; несуществующая — пустая.
    pub fn load(&self, date: Date) -> Result<Board> {
        storage::load(&self.page_path(date))
    }

    /// Сохраняет страницу. Пустая страница — это отсутствие файла: если все задачи удалены,
    /// файл удаляется, а пустой новый не создаётся.
    pub fn save(&self, date: Date, board: &Board) -> Result<()> {
        let path = self.page_path(date);
        if !board.is_empty() {
            return storage::save(&path, board);
        }
        match fs::remove_file(&path) {
            Err(e) if e.kind() != ErrorKind::NotFound => {
                Err(e).with_context(|| format!("удаление {}", path.display()))
            }
            _ => Ok(()),
        }
    }

    fn notes_path(&self, id: &str) -> PathBuf {
        self.dir.join("notes").join(format!("{id}.md"))
    }

    /// Записи задачи; нет файла — записей нет.
    pub fn load_notes(&self, id: &str) -> Result<Notes> {
        let path = self.notes_path(id);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Notes::parse(&text)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Notes::default()),
            Err(e) => Err(e).with_context(|| format!("чтение {}", path.display())),
        }
    }

    /// Сохраняет записи; пустые — без файла, как и пустая страница.
    pub fn save_notes(&self, id: &str, notes: &Notes) -> Result<()> {
        let path = self.notes_path(id);
        if !notes.is_empty() {
            return storage::write_atomic(&path, &notes.serialize());
        }
        match fs::remove_file(&path) {
            Err(e) if e.kind() != ErrorKind::NotFound => {
                Err(e).with_context(|| format!("удаление {}", path.display()))
            }
            _ => Ok(()),
        }
    }

    /// Путь к файлу записей для внешнего редактора; каталог создаётся заранее.
    pub fn notes_file(&self, id: &str) -> Result<PathBuf> {
        let path = self.notes_path(id);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("создание {}", dir.display()))?;
        }
        Ok(path)
    }

    /// Новый id записей, которого ещё нет в книжке: 6 знаков base36 из случайного хеша.
    pub fn new_id(&self) -> String {
        loop {
            let mut n = RandomState::new().hash_one(std::time::SystemTime::now());
            let id: String = (0..6)
                .map(|_| {
                    let c = char::from_digit((n % 36) as u32, 36).unwrap_or('0');
                    n /= 36;
                    c
                })
                .collect();
            if !self.notes_path(&id).exists() {
                return id;
            }
        }
    }

    /// Сводка записей для задач страницы — для значка `✎ 1/3`.
    pub fn summaries(&self, board: &Board) -> HashMap<String, Summary> {
        board
            .tasks()
            .filter_map(|t| t.id.clone())
            .filter_map(|id| {
                let summary = self.load_notes(&id).ok()?.summary();
                summary.any.then_some((id, summary))
            })
            .collect()
    }

    /// Есть ли задача с этим id хоть на одной сохранённой странице.
    pub fn is_referenced(&self, id: &str) -> Result<bool> {
        for date in self.dates()? {
            if self.load(date)?.tasks().any(|t| t.id.as_deref() == Some(id)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Удалена задача с записями: если её копий больше нет ни на одной странице, записи
    /// дописываются в `DELETED_NOTES.md`, а их файл удаляется. Возвращает, перенесены ли.
    pub fn archive_notes(&self, task: &Task, page: Date, stamp: &str) -> Result<bool> {
        let Some(id) = &task.id else {
            return Ok(false);
        };
        let notes = self.load_notes(id)?;
        if notes.is_empty() || self.is_referenced(id)? {
            return Ok(false);
        }
        self.append_deleted(&format!("{stamp} — {}", task.text), id, page, &notes.serialize())?;
        self.save_notes(id, &Notes::default())?;
        Ok(true)
    }

    /// Удалён один пункт записей (заметка, подзадача) — он дописывается в `DELETED_NOTES.md`.
    pub fn archive_entry(&self, task: &str, id: &str, page: Date, stamp: &str, raw: &str) -> Result<()> {
        self.append_deleted(&format!("{stamp} — {task} · удалённый пункт записей"), id, page, raw)
    }

    fn append_deleted(&self, title: &str, id: &str, page: Date, body: &str) -> Result<()> {
        let path = self.dir.join(DELETED_NOTES);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("запись {}", path.display()))?;
        write!(file, "## {title}\n<!-- id:{id}, страница {page} -->\n\n{body}\n")?;
        file.sync_all()?;
        Ok(())
    }

    /// Списки, скрытые от ИИ; нет файла — скрытых нет.
    pub fn hidden(&self) -> Result<[bool; 4]> {
        let path = self.dir.join(AI_HIDDEN);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok([false; 4]),
            Err(e) => return Err(e).with_context(|| format!("чтение {}", path.display())),
        };
        parse_hidden(&text).with_context(|| path.display().to_string())
    }

    pub fn set_hidden(&self, hidden: [bool; 4]) -> Result<()> {
        let mut text = String::from("# Списки, скрытые от ИИ\n\n");
        for category in Category::ALL.into_iter().filter(|c| hidden[c.index()]) {
            text.push_str(&format!("- {}\n", category.title()));
        }
        storage::write_atomic(&self.dir.join(AI_HIDDEN), &text)
    }

    pub fn hidden_stamp(&self) -> Option<Stamp> {
        stamp(&self.dir.join(AI_HIDDEN))
    }

    pub fn page_stamp(&self, date: Date) -> Option<Stamp> {
        stamp(&self.page_path(date))
    }

    pub fn notes_stamp(&self, id: &str) -> Option<Stamp> {
        stamp(&self.notes_path(id))
    }

    /// Даты всех страниц книжки, по возрастанию. Посторонние файлы не учитываются.
    pub fn dates(&self) -> Result<Vec<Date>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("чтение {}", self.dir.display())),
        };
        let mut dates: Vec<Date> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name();
                Date::parse_iso(name.to_str()?.strip_suffix(".md")?)
            })
            .collect();
        dates.sort();
        Ok(dates)
    }

    /// Автоперенос в начале дня: невыполненные задачи с последней заполненной страницы до
    /// `today` переезжают в конец тех же списков на `today`, а там помечаются `[>]`.
    /// Возвращает дату, с которой перенесли, и сколько задач.
    pub fn carry_over(&self, today: Date) -> Result<Option<(Date, usize)>> {
        let Some(from) = self.dates()?.into_iter().rev().find(|&d| d < today) else {
            return Ok(None);
        };
        let mut source = self.load(from)?;
        // Копии на сегодня — с теми же id, что и на исходной странице: если перенос сделают
        // два устройства сразу, при слиянии это будут одни и те же задачи, а не дубли.
        source.assign_ids(|| self.new_id());
        let moved = source.migrate_open(today);
        if moved.is_empty() {
            return Ok(None);
        }
        let count = moved.len();
        let mut target = self.load(today)?;
        for (category, task) in moved {
            target.push(category, task);
        }
        // Сначала новая страница: при сбое между записями задача окажется в двух местах,
        // а не пропадёт.
        self.save(today, &target)?;
        self.save(from, &source)?;
        Ok(Some((from, count)))
    }

    /// Ручной перенос: копия задачи — в конец того же списка на странице `to`, на текущей
    /// странице (`board`, ещё не сохранена) задача помечается `[>]`.
    pub fn postpone(
        &self,
        board: &mut Board,
        from: Date,
        category: Category,
        index: usize,
        to: Date,
    ) -> Result<bool> {
        board.assign_ids(|| self.new_id());
        let Some(task) = board.get_mut(category, index).filter(|t| t.is_open() && from != to)
        else {
            return Ok(false);
        };
        let mut target = self.load(to)?;
        target.push(category, task.carried());
        self.save(to, &target)?;
        task.state = State::Migrated(to);
        self.save(from, board)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(day: u32) -> Date {
        Date::new(2026, 10, day).unwrap()
    }

    fn temp_book(name: &str) -> Book {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-book-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Book::new(dir)
    }

    fn texts(board: &Board, category: Category) -> Vec<(String, State)> {
        board
            .list(category)
            .iter()
            .map(|t| (t.text.clone(), t.state))
            .collect()
    }

    #[test]
    fn empty_page_has_no_file() {
        let book = temp_book("empty");
        let mut board = Board::default();
        book.save(d(26), &board).unwrap();
        assert!(!book.page_path(d(26)).exists());

        board.push(Category::Work, Task::new("a"));
        book.save(d(26), &board).unwrap();
        assert!(book.page_path(d(26)).exists());
        fs::write(book.dir().join("notes.txt"), "").unwrap();
        assert_eq!(book.dates().unwrap(), [d(26)]);

        board.remove(Category::Work, 0);
        book.save(d(26), &board).unwrap();
        assert!(!book.page_path(d(26)).exists());
        assert!(book.dates().unwrap().is_empty());
        fs::remove_dir_all(book.dir()).unwrap();
    }

    #[test]
    fn carry_over_from_last_page() {
        let book = temp_book("carry");
        assert_eq!(book.carry_over(d(26)).unwrap(), None);

        let mut older = Board::default();
        older.push(Category::Home, Task::new("старая"));
        book.save(d(20), &older).unwrap();

        let mut last = Board::default();
        last.push(Category::Work, Task::new("доделать"));
        last.push(Category::Work, Task::new("готово"));
        last.get_mut(Category::Work, 1).unwrap().state = State::Done;
        last.push(Category::Personal, Task::new("позвонить"));
        book.save(d(23), &last).unwrap();

        // Страница на сегодня уже заполнена заранее: перенесённое — в конец.
        let mut today = Board::default();
        today.push(Category::Work, Task::new("план"));
        book.save(d(26), &today).unwrap();

        assert_eq!(book.carry_over(d(26)).unwrap(), Some((d(23), 2)));
        let today = book.load(d(26)).unwrap();
        assert_eq!(
            texts(&today, Category::Work),
            [("план".into(), State::Open), ("доделать".into(), State::Open)]
        );
        assert_eq!(texts(&today, Category::Personal), [("позвонить".into(), State::Open)]);
        let last = book.load(d(23)).unwrap();
        assert_eq!(
            texts(&last, Category::Work),
            [
                ("доделать".into(), State::Migrated(d(26))),
                ("готово".into(), State::Done)
            ]
        );
        // Только с последней страницы; повторный запуск ничего не делает.
        assert_eq!(book.load(d(20)).unwrap(), older);
        assert_eq!(book.carry_over(d(26)).unwrap(), None);
        fs::remove_dir_all(book.dir()).unwrap();
    }

    #[test]
    fn notes_follow_task_and_archive() {
        let book = temp_book("notes");
        let id = book.new_id();
        assert_eq!(id.len(), 6);
        let mut notes = Notes::default();
        notes.add_subtask("купить");
        book.save_notes(&id, &notes).unwrap();
        assert_eq!(book.load_notes(&id).unwrap(), notes);

        let mut task = Task::new("кран");
        task.id = Some(id.clone());
        let mut page = Board::default();
        page.push(Category::Home, task.clone());
        book.save(d(25), &page).unwrap();
        assert_eq!(book.summaries(&page)[&id].subtasks, 1);

        // Перенос: копия с тем же id.
        assert_eq!(book.carry_over(d(26)).unwrap(), Some((d(25), 1)));
        let today = book.load(d(26)).unwrap();
        assert_eq!(today.list(Category::Home)[0].id.as_deref(), Some(id.as_str()));

        // Удалили копию на 26-м — запись на 25-м осталась, записи живы.
        book.save(d(26), &Board::default()).unwrap();
        assert!(!book.archive_notes(&task, d(26), "2026-10-26 19:40").unwrap());
        assert!(book.notes_path(&id).exists());

        // Удалили последнюю — записи в DELETED_NOTES.md.
        book.save(d(25), &Board::default()).unwrap();
        assert!(book.archive_notes(&task, d(25), "2026-10-26 19:41").unwrap());
        assert!(!book.notes_path(&id).exists());
        let deleted = fs::read_to_string(book.dir().join(DELETED_NOTES)).unwrap();
        assert!(deleted.starts_with("## 2026-10-26 19:41 — кран\n"), "{deleted}");
        assert!(deleted.contains(&format!("id:{id}, страница 2026-10-25")));
        assert!(deleted.contains("- [ ] купить\n"));
        // Пустые записи — без файла.
        book.save_notes(&id, &Notes::default()).unwrap();
        assert!(!book.notes_path(&id).exists());
        fs::remove_dir_all(book.dir()).unwrap();
    }

    #[test]
    fn hidden_lists_and_entry_archive() {
        let book = temp_book("hidden");
        assert_eq!(book.hidden().unwrap(), [false; 4]);
        book.set_hidden([false, false, true, false]).unwrap();
        assert_eq!(book.hidden().unwrap(), [false, false, true, false]);
        let text = fs::read_to_string(book.dir().join(AI_HIDDEN)).unwrap();
        assert!(text.contains("- Личное\n"), "{text}");
        fs::write(book.dir().join(AI_HIDDEN), "- Отпуск\n").unwrap();
        assert!(book.hidden().is_err());

        book.archive_entry("кран", "abc", d(26), "2026-10-26 21:00", "- [ ] купить\n").unwrap();
        let deleted = fs::read_to_string(book.dir().join(DELETED_NOTES)).unwrap();
        assert!(deleted.starts_with("## 2026-10-26 21:00 — кран · удалённый пункт записей\n"));
        assert!(deleted.ends_with("\n\n- [ ] купить\n\n"), "{deleted:?}");

        assert_eq!(book.page_stamp(d(26)), None);
        let mut board = Board::default();
        board.push(Category::Work, Task::new("a"));
        book.save(d(26), &board).unwrap();
        assert!(book.page_stamp(d(26)).is_some());
        fs::remove_dir_all(book.dir()).unwrap();
    }

    #[test]
    fn postpone_to_date() {
        let book = temp_book("postpone");
        let mut board = Board::default();
        board.push(Category::Other, Task::new("в пятницу"));
        board.push(Category::Other, Task::new("сделано"));
        board.get_mut(Category::Other, 1).unwrap().state = State::Done;

        assert!(book.postpone(&mut board, d(26), Category::Other, 0, d(30)).unwrap());
        assert_eq!(board.list(Category::Other)[0].state, State::Migrated(d(30)));
        assert_eq!(book.load(d(26)).unwrap(), board);
        assert_eq!(
            texts(&book.load(d(30)).unwrap(), Category::Other),
            [("в пятницу".into(), State::Open)]
        );
        // Выполненную, уже перенесённую и на ту же дату — нельзя.
        assert!(!book.postpone(&mut board, d(26), Category::Other, 1, d(30)).unwrap());
        assert!(!book.postpone(&mut board, d(26), Category::Other, 0, d(31)).unwrap());
        let mut fresh = Board::default();
        fresh.push(Category::Work, Task::new("x"));
        assert!(!book.postpone(&mut fresh, d(26), Category::Work, 0, d(26)).unwrap());
        fs::remove_dir_all(book.dir()).unwrap();
    }
}
