//! Книжка: страницы по датам (`2026-10-26.md` — списки дел на этот день), записи задач
//! (`notes/<id>.md`), реестр списков (`LISTS.md`), архив удалённого. Файлы — через `Store`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::clock::Date;
use crate::lists::{LISTS_FILE, Lists};
use crate::model::{Board, ListId, State, Task};
use crate::notes::{Notes, Summary};
use crate::storage;
pub use crate::store::Stamp;
use crate::store::{FsStore, Store};

/// Куда уходят удалённые записи, чтобы не терять их.
pub const DELETED_NOTES: &str = "DELETED_NOTES.md";
/// Скрытые от ИИ списки в прежних версиях: `- Личное` по строке на список. Теперь это
/// пометка в `LISTS.md`; файл переносится туда при запуске (`Book::migrate`) и удаляется.
pub const AI_HIDDEN: &str = "AI_HIDDEN.md";

/// Списки из текста `AI_HIDDEN.md`.
pub fn parse_hidden(text: &str, lists: &Lists) -> Result<Vec<ListId>> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("- "))
        .map(|line| lists.resolve_heading(line).with_context(|| format!("неизвестный список «{line}»")))
        .collect()
}

/// Какой это файл книжки — по пути относительно её каталога.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileKind {
    Page(Date),
    Notes(String),
    Lists,
    Deleted,
}

pub fn file_kind(path: &str) -> Option<FileKind> {
    match path.strip_prefix("notes/") {
        Some(name) => {
            let id = name.strip_suffix(".md").filter(|id| storage::is_valid_id(id))?;
            Some(FileKind::Notes(id.to_string()))
        }
        None if path == LISTS_FILE => Some(FileKind::Lists),
        None if path == DELETED_NOTES => Some(FileKind::Deleted),
        None => path.strip_suffix(".md").and_then(Date::parse_iso).map(FileKind::Page),
    }
}

fn page_path(date: Date) -> String {
    format!("{date}.md")
}

fn notes_path(id: &str) -> String {
    format!("notes/{id}.md")
}

/// Прочитанный реестр списков: отпечаток файла, реестр и ошибка разбора, если файл испорчен
/// (тогда вместо него — исходные списки, а записывать реестр нельзя).
struct ListsCache {
    stamp: Option<Stamp>,
    lists: Lists,
    error: Option<String>,
}

pub struct Book {
    /// Каталог книжки: для показа, внешнего редактора и служебных файлов синхронизации.
    dir: PathBuf,
    store: Box<dyn Store>,
    lists: RefCell<Option<ListsCache>>,
    /// Разобранные страницы по отпечатку файла: история, поиск и превью читают книжку
    /// целиком, а меняется в ней за раз одна-две страницы.
    pages: RefCell<HashMap<Date, (Stamp, Board)>>,
}

impl Book {
    pub fn new(dir: PathBuf) -> Self {
        let store = Box::new(FsStore::new(dir.clone()));
        Self::with_store(dir, store)
    }

    pub fn with_store(dir: PathBuf, store: Box<dyn Store>) -> Self {
        Self { dir, store, lists: RefCell::new(None), pages: RefCell::new(HashMap::new()) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    // ------------------------------------------------------------------ файлы

    /// Файл книжки как есть (для синхронизации); нет файла — `None`.
    pub fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        self.store.read(path)
    }

    pub fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        self.store.write(path, data)
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        self.store.remove(path)
    }

    /// Все файлы книжки (страницы, записи, реестр, архив) — пути относительно каталога.
    pub fn files(&self) -> Result<Vec<String>> {
        let mut files = Vec::new();
        for (dir, prefix) in [("", ""), ("notes", "notes/")] {
            for name in self.store.list(dir)? {
                let path = format!("{prefix}{name}");
                if file_kind(&path).is_some() {
                    files.push(path);
                }
            }
        }
        Ok(files)
    }

    fn read_text(&self, path: &str) -> Result<Option<String>> {
        self.store
            .read(path)?
            .map(|data| String::from_utf8(data).with_context(|| format!("{path}: не текст в UTF-8")))
            .transpose()
    }

    /// Записывает текст; пустой — удаляет файл (пустая страница — это отсутствие файла).
    fn write_or_remove(&self, path: &str, text: &str) -> Result<()> {
        if text.is_empty() { self.store.remove(path) } else { self.store.write(path, text.as_bytes()) }
    }

    // ------------------------------------------------------------------ страницы

    /// Страница на дату; несуществующая — пустая.
    pub fn load(&self, date: Date) -> Result<Board> {
        let path = page_path(date);
        let Some(stamp) = self.store.stamp(&path) else {
            return Ok(Board::default());
        };
        if let Some((cached, board)) = self.pages.borrow().get(&date)
            && *cached == stamp
        {
            return Ok(board.clone());
        }
        let text = self.read_text(&path)?.unwrap_or_default();
        let board = self.with_lists(|lists| storage::parse(&text, lists)).with_context(|| format!("разбор {path}"))?;
        self.pages.borrow_mut().insert(date, (stamp, board.clone()));
        Ok(board)
    }

    /// Сохраняет страницу. Пустая страница — это отсутствие файла: если все задачи удалены,
    /// файл удаляется, а пустой новый не создаётся.
    pub fn save(&self, date: Date, board: &Board) -> Result<()> {
        if board.is_empty() {
            return self.store.remove(&page_path(date));
        }
        let text = self.with_lists(|lists| Ok(storage::serialize(board, lists)))?;
        self.store.write(&page_path(date), text.as_bytes())
    }

    pub fn page_stamp(&self, date: Date) -> Option<Stamp> {
        self.store.stamp(&page_path(date))
    }

    /// Даты всех страниц книжки, по возрастанию. Посторонние файлы не учитываются.
    pub fn dates(&self) -> Result<Vec<Date>> {
        let mut dates: Vec<Date> =
            self.store.list("")?.iter().filter_map(|name| Date::parse_iso(name.strip_suffix(".md")?)).collect();
        dates.sort();
        Ok(dates)
    }

    // ------------------------------------------------------------------ реестр списков

    /// Свежий реестр (перечитывается, если файл поменялся) — для `f`, без копирования.
    fn with_lists<T>(&self, f: impl FnOnce(&Lists) -> Result<T>) -> Result<T> {
        let stamp = self.store.stamp(LISTS_FILE);
        let fresh = self.lists.borrow().as_ref().is_some_and(|c| c.stamp == stamp);
        if !fresh {
            let (lists, error) = match self.read_text(LISTS_FILE)? {
                None => (Lists::default(), None),
                Some(text) => match Lists::parse(&text) {
                    Ok(lists) => (lists, None),
                    // Испорченный реестр не должен делать книжку непригодной: страницы
                    // читаются с исходными списками (незнакомые id — с названиями из
                    // заголовков), а сам файл не перезаписывается, пока его не исправят.
                    Err(e) => (Lists::default(), Some(format!("{LISTS_FILE}: {e:#}"))),
                },
            };
            *self.lists.borrow_mut() = Some(ListsCache { stamp, lists, error });
        }
        let cache = self.lists.borrow();
        f(&cache.as_ref().unwrap().lists)
    }

    /// Реестр списков; нет файла — списки новой книжки.
    pub fn lists(&self) -> Result<Lists> {
        self.with_lists(|lists| Ok(lists.clone()))
    }

    /// Испорчен ли `LISTS.md` (тогда в ходу исходные списки и менять реестр нельзя).
    pub fn lists_error(&self) -> Option<String> {
        self.with_lists(|_| Ok(())).ok()?;
        self.lists.borrow().as_ref()?.error.clone()
    }

    pub fn save_lists(&self, lists: &Lists) -> Result<()> {
        if let Some(e) = self.lists_error() {
            bail!("{e} — исправьте файл, иначе изменение его затрёт");
        }
        self.store.write(LISTS_FILE, lists.serialize().as_bytes())?;
        *self.lists.borrow_mut() =
            Some(ListsCache { stamp: self.store.stamp(LISTS_FILE), lists: lists.clone(), error: None });
        Ok(())
    }

    pub fn lists_stamp(&self) -> Option<Stamp> {
        self.store.stamp(LISTS_FILE)
    }

    /// Переводит книжку прежних версий на нынешний формат — один раз, при запуске, а не при
    /// чтении: скрытые от ИИ списки из `AI_HIDDEN.md` — в `LISTS.md`. Возвращает, что сделано.
    pub fn migrate(&self) -> Result<Vec<String>> {
        let mut done = Vec::new();
        if let Some(text) = self.read_text(AI_HIDDEN)? {
            let mut lists = self.lists()?;
            for id in parse_hidden(&text, &lists).with_context(|| AI_HIDDEN.to_string())? {
                lists.set_hidden(id, true)?;
            }
            self.save_lists(&lists)?;
            self.store.remove(AI_HIDDEN)?;
            done.push(format!("{AI_HIDDEN} → {LISTS_FILE}"));
        }
        Ok(done)
    }

    /// Были ли в списке задачи хоть на одной странице.
    pub fn list_used(&self, list: ListId) -> Result<bool> {
        for date in self.dates()? {
            if !self.load(date)?.list(list).is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Сколько задач в списке на страницах с `since` и дальше.
    pub fn tasks_since(&self, list: ListId, since: Date) -> Result<usize> {
        let mut count = 0;
        for date in self.dates()?.into_iter().filter(|&d| d >= since) {
            count += self.load(date)?.list(list).len();
        }
        Ok(count)
    }

    /// Удаление списка: все его задачи на страницах с `since` и дальше — в конец списка `to`.
    /// Прошлые страницы — история, их не трогаем. Возвращает, сколько перенесено.
    pub fn move_list_tasks(&self, from: ListId, to: ListId, since: Date) -> Result<usize> {
        let mut count = 0;
        for date in self.dates()?.into_iter().filter(|&d| d >= since) {
            let mut board = self.load(date)?;
            let tasks = board.list(from).len();
            if tasks == 0 {
                continue;
            }
            for _ in 0..tasks {
                board.move_to(from, 0, to);
            }
            self.save(date, &board)?;
            count += tasks;
        }
        Ok(count)
    }

    // ------------------------------------------------------------------ записи задач

    /// Записи задачи; нет файла — записей нет.
    pub fn load_notes(&self, id: &str) -> Result<Notes> {
        Ok(self.read_text(&notes_path(id))?.map(|text| Notes::parse(&text)).unwrap_or_default())
    }

    /// Сохраняет записи; пустые — без файла, как и пустая страница.
    pub fn save_notes(&self, id: &str, notes: &Notes) -> Result<()> {
        let text = if notes.is_empty() { String::new() } else { notes.serialize() };
        self.write_or_remove(&notes_path(id), &text)
    }

    pub fn notes_stamp(&self, id: &str) -> Option<Stamp> {
        self.store.stamp(&notes_path(id))
    }

    /// Путь к файлу записей для внешнего редактора; каталог создаётся заранее.
    pub fn notes_file(&self, id: &str) -> Result<PathBuf> {
        let path = self.dir.join(notes_path(id));
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("создание {}", dir.display()))?;
        }
        Ok(path)
    }

    /// Новый id (задачи, списка): 6 знаков base36 из `/dev/urandom` (36⁶ ≈ 2 млрд), которого
    /// нет среди записей и списков.
    pub fn new_id(&self) -> String {
        let lists = self.lists().unwrap_or_default();
        loop {
            let mut bytes = [0u8; 8];
            let random = fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_ok();
            let mut n = if random {
                u64::from_le_bytes(bytes)
            } else {
                // Нет /dev/urandom — время с наносекундами и адрес на стеке.
                let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                t.as_nanos() as u64 ^ (&bytes as *const _ as u64).rotate_left(32)
            };
            let id: String = (0..6)
                .map(|_| {
                    let c = char::from_digit((n % 36) as u32, 36).unwrap_or('0');
                    n /= 36;
                    c
                })
                .collect();
            if self.store.stamp(&notes_path(&id)).is_none() && lists.get(ListId::new(&id)).is_none() {
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

    /// История задачи: все страницы, где она есть (перенос сохраняет id), по датам.
    pub fn history(&self, id: &str) -> Result<Vec<(Date, ListId, State)>> {
        let mut history = Vec::new();
        for date in self.dates()? {
            let board = self.load(date)?;
            for category in board.ids() {
                for t in board.list(category).iter().filter(|t| t.id.as_deref() == Some(id)) {
                    history.push((date, category, t.state));
                }
            }
        }
        Ok(history)
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
        let mut text = self.read_text(DELETED_NOTES)?.unwrap_or_default();
        text.push_str(&format!("## {title}\n<!-- id:{id}, страница {page} -->\n\n{body}\n"));
        self.store.write(DELETED_NOTES, text.as_bytes())
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
        // Список удалили, а задачи в нём остались на старой странице — в первый список.
        let lists = self.lists()?;
        for (category, task) in moved {
            let category = if lists.get(category).is_some_and(|d| d.archived) { lists.active()[0] } else { category };
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
    pub fn postpone(&self, board: &mut Board, from: Date, category: ListId, index: usize, to: Date) -> Result<bool> {
        board.assign_ids(|| self.new_id());
        let Some(task) = board.get_mut(category, index).filter(|t| t.is_open() && from != to) else {
            return Ok(false);
        };
        let mut target = self.load(to)?;
        target.push(category, task.carried());
        self.save(to, &target)?;
        task.state = State::Migrated(to);
        task.touch();
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

    fn texts(board: &Board, category: ListId) -> Vec<(String, State)> {
        board.list(category).iter().map(|t| (t.text.clone(), t.state)).collect()
    }

    #[test]
    fn empty_page_has_no_file() {
        let book = temp_book("empty");
        let mut board = Board::default();
        book.save(d(26), &board).unwrap();
        assert!(book.page_stamp(d(26)).is_none());

        board.push(ListId::WORK, Task::new("a"));
        book.save(d(26), &board).unwrap();
        assert!(book.page_stamp(d(26)).is_some());
        fs::write(book.dir().join("notes.txt"), "").unwrap();
        assert_eq!(book.dates().unwrap(), [d(26)]);

        board.remove(ListId::WORK, 0);
        book.save(d(26), &board).unwrap();
        assert!(book.page_stamp(d(26)).is_none());
        assert!(book.dates().unwrap().is_empty());
        fs::remove_dir_all(book.dir()).unwrap();
    }

    #[test]
    fn carry_over_from_last_page() {
        let book = temp_book("carry");
        assert_eq!(book.carry_over(d(26)).unwrap(), None);

        let mut older = Board::default();
        older.push(ListId::HOME, Task::new("старая"));
        book.save(d(20), &older).unwrap();

        let mut last = Board::default();
        last.push(ListId::WORK, Task::new("доделать"));
        last.push(ListId::WORK, Task::new("готово"));
        last.get_mut(ListId::WORK, 1).unwrap().state = State::Done;
        last.push(ListId::PERSONAL, Task::new("позвонить"));
        book.save(d(23), &last).unwrap();

        // Страница на сегодня уже заполнена заранее: перенесённое — в конец.
        let mut today = Board::default();
        today.push(ListId::WORK, Task::new("план"));
        book.save(d(26), &today).unwrap();

        assert_eq!(book.carry_over(d(26)).unwrap(), Some((d(23), 2)));
        let today = book.load(d(26)).unwrap();
        assert_eq!(texts(&today, ListId::WORK), [("план".into(), State::Open), ("доделать".into(), State::Open)]);
        assert_eq!(texts(&today, ListId::PERSONAL), [("позвонить".into(), State::Open)]);
        let last = book.load(d(23)).unwrap();
        assert_eq!(
            texts(&last, ListId::WORK),
            [("доделать".into(), State::Migrated(d(26))), ("готово".into(), State::Done)]
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
        page.push(ListId::HOME, task.clone());
        book.save(d(25), &page).unwrap();
        assert_eq!(book.summaries(&page)[&id].subtasks, 1);

        // Перенос: копия с тем же id.
        assert_eq!(book.carry_over(d(26)).unwrap(), Some((d(25), 1)));
        let today = book.load(d(26)).unwrap();
        assert_eq!(today.list(ListId::HOME)[0].id.as_deref(), Some(id.as_str()));

        // Удалили копию на 26-м — запись на 25-м осталась, записи живы.
        book.save(d(26), &Board::default()).unwrap();
        assert!(!book.archive_notes(&task, d(26), "26.10.2026 19:40").unwrap());
        assert!(book.notes_stamp(&id).is_some());

        // Удалили последнюю — записи в DELETED_NOTES.md.
        book.save(d(25), &Board::default()).unwrap();
        assert!(book.archive_notes(&task, d(25), "26.10.2026 19:41").unwrap());
        assert!(book.notes_stamp(&id).is_none());
        let deleted = fs::read_to_string(book.dir().join(DELETED_NOTES)).unwrap();
        assert!(deleted.starts_with("## 26.10.2026 19:41 — кран\n"), "{deleted}");
        assert!(deleted.contains(&format!("id:{id}, страница 2026-10-25")));
        assert!(deleted.contains("- [ ] купить\n"));
        // Пустые записи — без файла.
        book.save_notes(&id, &Notes::default()).unwrap();
        assert!(book.notes_stamp(&id).is_none());
        fs::remove_dir_all(book.dir()).unwrap();
    }

    #[test]
    fn deleting_a_list() {
        let book = temp_book("dellist");
        let repair = ListId::new("rep1");
        let mut lists = book.lists().unwrap();
        lists.add(repair, "Ремонт").unwrap();
        book.save_lists(&lists).unwrap();
        for (day, text) in [(25, "замер"), (26, "плитка"), (28, "сантехник")] {
            let mut board = book.load(d(day)).unwrap();
            board.push(ListId::WORK, Task::new("работа"));
            board.push(repair, Task::new(text));
            book.save(d(day), &board).unwrap();
        }
        assert!(book.list_used(repair).unwrap() && !book.list_used(ListId::OTHER).unwrap());
        assert_eq!(book.tasks_since(repair, d(26)).unwrap(), 2);
        // С 26-го и дальше задачи уезжают в «Дом», 25-е — история.
        assert_eq!(book.move_list_tasks(repair, ListId::HOME, d(26)).unwrap(), 2);
        assert_eq!(texts(&book.load(d(25)).unwrap(), repair), [("замер".into(), State::Open)]);
        assert!(book.load(d(26)).unwrap().list(repair).is_empty());
        assert_eq!(texts(&book.load(d(28)).unwrap(), ListId::HOME), [("сантехник".into(), State::Open)]);

        // Автоперенос из архивного списка — в первый список на экране.
        lists.archive(repair).unwrap();
        book.save_lists(&lists).unwrap();
        let mut board = book.load(d(28)).unwrap();
        board.push(repair, Task::new("забытая"));
        book.save(d(28), &board).unwrap();
        let text = fs::read_to_string(book.dir().join("2026-10-28.md")).unwrap();
        assert!(text.contains("## Ремонт <!-- list:rep1 -->\n- [ ] забытая"), "{text}");
        book.carry_over(d(29)).unwrap();
        let today = book.load(d(29)).unwrap();
        assert!(today.list(repair).is_empty());
        assert!(today.list(ListId::WORK).iter().any(|t| t.text == "забытая"));
        fs::remove_dir_all(book.dir()).unwrap();
    }

    #[test]
    fn memory_store() {
        // Та же книжка, но без диска: перенос, записи, кэш страниц.
        let book = Book::with_store(PathBuf::from("mem"), Box::new(crate::store::MemStore::default()));
        let mut board = Board::default();
        board.push(ListId::WORK, Task::new("доделать"));
        book.save(d(26), &board).unwrap();
        assert_eq!(book.carry_over(d(27)).unwrap(), Some((d(26), 1)));
        assert_eq!(texts(&book.load(d(27)).unwrap(), ListId::WORK), [("доделать".into(), State::Open)]);
        assert_eq!(book.load(d(26)).unwrap().list(ListId::WORK)[0].state, State::Migrated(d(27)));
        assert_eq!(book.dates().unwrap(), [d(26), d(27)]);
        let id = book.load(d(27)).unwrap().list(ListId::WORK)[0].id.clone().unwrap();
        assert_eq!(book.history(&id).unwrap().len(), 2);
        let mut notes = Notes::default();
        notes.add_subtask("шаг");
        book.save_notes(&id, &notes).unwrap();
        assert_eq!(book.files().unwrap().len(), 3);
        assert!(!PathBuf::from("mem").exists());
    }

    #[test]
    fn hidden_lists_and_entry_archive() {
        let book = temp_book("hidden");
        // Нет реестра — списки новой книжки, файл появляется при первом изменении.
        assert_eq!(book.lists().unwrap(), Lists::default());
        assert!(book.lists_stamp().is_none());
        // Скрытые списки прежних версий переезжают в реестр явной миграцией, не чтением.
        fs::create_dir_all(book.dir()).unwrap();
        fs::write(book.dir().join(AI_HIDDEN), "# Списки, скрытые от ИИ\n\n- Личное\n").unwrap();
        assert!(!book.lists().unwrap().is_hidden(ListId::PERSONAL));
        assert_eq!(book.migrate().unwrap().len(), 1);
        assert!(book.lists().unwrap().is_hidden(ListId::PERSONAL));
        assert!(!book.dir().join(AI_HIDDEN).exists());
        assert!(book.migrate().unwrap().is_empty());
        let text = fs::read_to_string(book.dir().join(LISTS_FILE)).unwrap();
        assert!(
            text.lines().any(|l| l.starts_with("- Личное <!-- list:personal ") && l.ends_with(" hidden -->")),
            "{text}"
        );
        fs::write(book.dir().join(AI_HIDDEN), "- Отпуск\n").unwrap();
        assert!(book.migrate().is_err());
        fs::remove_file(book.dir().join(AI_HIDDEN)).unwrap();

        // Испорченный реестр: страницы читаются с исходными списками, реестр не затирается.
        let good = fs::read_to_string(book.dir().join(LISTS_FILE)).unwrap();
        fs::write(book.dir().join(LISTS_FILE), "мусор\n").unwrap();
        assert!(book.lists_error().is_some_and(|e| e.contains("LISTS.md")));
        assert_eq!(book.lists().unwrap(), Lists::default());
        let mut board = Board::default();
        board.push(ListId::WORK, Task::new("читается"));
        book.save(d(20), &board).unwrap();
        assert_eq!(book.load(d(20)).unwrap(), board);
        assert!(book.save_lists(&Lists::default()).is_err());
        assert_eq!(fs::read_to_string(book.dir().join(LISTS_FILE)).unwrap(), "мусор\n");
        fs::write(book.dir().join(LISTS_FILE), good).unwrap();
        book.save(d(20), &Board::default()).unwrap();
        assert!(book.lists_error().is_none());
        // Реестр поменяли снаружи — читается заново.
        let mut lists = book.lists().unwrap();
        lists.rename(ListId::HOME, "Квартира").unwrap();
        fs::write(book.dir().join(LISTS_FILE), lists.serialize()).unwrap();
        assert_eq!(book.lists().unwrap().title(ListId::HOME), Some("Квартира"));

        book.archive_entry("кран", "abc", d(26), "26.10.2026 21:00", "- [ ] купить\n").unwrap();
        let deleted = fs::read_to_string(book.dir().join(DELETED_NOTES)).unwrap();
        assert!(deleted.starts_with("## 26.10.2026 21:00 — кран · удалённый пункт записей\n"));
        assert!(deleted.ends_with("\n\n- [ ] купить\n\n"), "{deleted:?}");

        assert_eq!(book.page_stamp(d(26)), None);
        let mut board = Board::default();
        board.push(ListId::WORK, Task::new("a"));
        book.save(d(26), &board).unwrap();
        assert!(book.page_stamp(d(26)).is_some());
        fs::remove_dir_all(book.dir()).unwrap();
    }

    #[test]
    fn postpone_to_date() {
        let book = temp_book("postpone");
        let mut board = Board::default();
        board.push(ListId::OTHER, Task::new("в пятницу"));
        board.push(ListId::OTHER, Task::new("сделано"));
        board.get_mut(ListId::OTHER, 1).unwrap().state = State::Done;

        assert!(book.postpone(&mut board, d(26), ListId::OTHER, 0, d(30)).unwrap());
        assert_eq!(board.list(ListId::OTHER)[0].state, State::Migrated(d(30)));
        assert_eq!(book.load(d(26)).unwrap(), board);
        assert_eq!(texts(&book.load(d(30)).unwrap(), ListId::OTHER), [("в пятницу".into(), State::Open)]);
        // Выполненную, уже перенесённую и на ту же дату — нельзя.
        assert!(!book.postpone(&mut board, d(26), ListId::OTHER, 1, d(30)).unwrap());
        assert!(!book.postpone(&mut board, d(26), ListId::OTHER, 0, d(31)).unwrap());
        let mut fresh = Board::default();
        fresh.push(ListId::WORK, Task::new("x"));
        assert!(!book.postpone(&mut fresh, d(26), ListId::WORK, 0, d(26)).unwrap());
        fs::remove_dir_all(book.dir()).unwrap();
    }
}
