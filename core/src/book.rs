//! Книжка: страницы по датам (`2026-10-26.md` — списки дел на этот день), записи задач
//! (`notes/<id>.md`), реестр списков (`LISTS.md`), архив удалённого. Файлы — через `Store`.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

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
    lock_depth: Cell<usize>,
    lock_file: RefCell<Option<fs::File>>,
}

/// Короткая локальная транзакция. Вложенные блокировки того же `Book` безопасны.
pub struct BookLock<'a> {
    book: &'a Book,
}

impl Drop for BookLock<'_> {
    fn drop(&mut self) {
        let depth = self.book.lock_depth.get() - 1;
        self.book.lock_depth.set(depth);
        if depth == 0 {
            self.book.lock_file.borrow_mut().take();
        }
    }
}

impl Book {
    pub fn new(dir: PathBuf) -> Self {
        let store = Box::new(FsStore::new(dir.clone()));
        Self::with_store(dir, store)
    }

    pub fn with_store(dir: PathBuf, store: Box<dyn Store>) -> Self {
        Self {
            dir,
            store,
            lists: RefCell::new(None),
            pages: RefCell::new(HashMap::new()),
            lock_depth: Cell::new(0),
            lock_file: RefCell::new(None),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Общая межпроцессная граница read–modify–write; не держать во время сети/ввода.
    /// Для нескольких `Book` одного каталога брать только один guard, без вложения.
    pub fn lock(&self) -> Result<BookLock<'_>> {
        if self.lock_depth.get() == 0 {
            *self.lock_file.borrow_mut() = self.store.lock()?;
        }
        self.lock_depth.set(self.lock_depth.get() + 1);
        Ok(BookLock { book: self })
    }

    // ------------------------------------------------------------------ файлы

    /// Файл книжки как есть (для синхронизации); нет файла — `None`.
    pub fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        self.store.read(path)
    }

    pub fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        let _guard = self.lock()?;
        self.store.write(path, data)
    }

    pub fn remove_file(&self, path: &str) -> Result<()> {
        let _guard = self.lock()?;
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
        let stamp = self.store.stamp(&path);
        if let Some((cached, board)) = self.pages.borrow().get(&date)
            && Some(*cached) == stamp
        {
            return Ok(board.clone());
        }
        let text = self.read_text(&path)?.unwrap_or_default();
        let board = self.with_lists(|lists| storage::parse(&text, lists)).with_context(|| format!("разбор {path}"))?;
        board.validate_ids()?;
        board.revision.set(Some((date, stamp)));
        if let Some(stamp) = stamp {
            self.pages.borrow_mut().insert(date, (stamp, board.clone()));
        }
        Ok(board)
    }

    /// Условное сохранение снимка из `load`: устаревший снимок возвращает конфликт.
    /// Для открытого черновика со слиянием используйте `save_page`.
    pub fn save(&self, date: Date, board: &Board) -> Result<()> {
        let _guard = self.lock()?;
        board.validate_ids()?;
        if let Some((loaded_date, stamp)) = board.revision.get()
            && loaded_date == date
            && self.page_stamp(date) != stamp
        {
            bail!("страница {date} изменилась: перечитайте её перед сохранением");
        }
        // При ошибке metadata отсутствие файла подтверждается чтением.
        if self.page_stamp(date).is_none() {
            self.read_file(&page_path(date))?;
        }
        if board.is_empty() && board.graves().is_empty() {
            self.store.remove(&page_path(date))?;
        } else {
            let text = self.with_lists(|lists| Ok(storage::serialize(board, lists)))?;
            self.store.write(&page_path(date), text.as_bytes())?;
        }
        board.revision.set(Some((date, self.page_stamp(date))));
        self.pages.borrow_mut().remove(&date);
        Ok(())
    }

    /// Согласованный снимок страницы и её отпечатка для открытого черновика.
    pub fn open_page(&self, date: Date) -> Result<(Board, Option<Stamp>)> {
        let _guard = self.lock()?;
        Ok((self.load(date)?, self.page_stamp(date)))
    }

    /// Commit открытого черновика: check, parse, merge и запись под одним guard.
    /// При ошибке черновик и подтверждённый stamp остаются прежними.
    pub fn save_page(&self, date: Date, board: &mut Board, stamp: &mut Option<Stamp>) -> Result<()> {
        let _guard = self.lock()?;
        board.validate_ids()?;
        let current = self.page_stamp(date);
        let mut candidate = if current != *stamp {
            crate::merge::merge_boards(board, &self.load(date)?)
        } else {
            // Чтение обязательно и при stamp=None: это не доказательство отсутствия файла.
            self.read_file(&page_path(date))?;
            board.clone()
        };
        candidate.revision.set(Some((date, current)));
        self.assign_ids(&mut candidate)?;
        self.save(date, &candidate)?;
        *stamp = self.page_stamp(date);
        *board = candidate;
        Ok(())
    }

    /// Назначение id с проверкой уже занятых идентичностей, включая несохранённую доску.
    pub fn assign_ids(&self, board: &mut Board) -> Result<bool> {
        let _guard = self.lock()?;
        board.validate_ids()?;
        self.reserve_ids(
            board.tasks().filter_map(|task| task.id.clone()).chain(board.graves().iter().map(|(id, _)| id.clone())),
        )?;
        if !board.tasks().any(|task| task.id.is_none()) {
            return Ok(false);
        }
        self.allocate_ids(|random, reserved| {
            for category in board.ids() {
                for index in 0..board.list(category).len() {
                    let task = board.get_mut(category, index).unwrap();
                    if task.id.is_none() {
                        task.id = Some(fresh_id(random, reserved)?);
                    }
                }
            }
            Ok(true)
        })
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
        let _guard = self.lock()?;
        if let Some(e) = self.lists_error() {
            bail!("{e} — исправьте файл, иначе изменение его затрёт");
        }
        self.store.write(LISTS_FILE, lists.serialize().as_bytes())?;
        *self.lists.borrow_mut() =
            Some(ListsCache { stamp: self.store.stamp(LISTS_FILE), lists: lists.clone(), error: None });
        Ok(())
    }

    /// Применяет правку к актуальному реестру в общей локальной транзакции.
    pub fn edit_lists(&self, edit: impl FnOnce(&mut Lists) -> Result<()>) -> Result<Lists> {
        let _guard = self.lock()?;
        let mut lists = self.lists()?;
        edit(&mut lists)?;
        self.save_lists(&lists)?;
        Ok(lists)
    }

    /// Удаляет/архивирует список и переносит актуальные задачи в одной транзакции.
    pub fn delete_list(&self, list: ListId, to: Option<ListId>, since: Date) -> Result<(Lists, usize, bool)> {
        let _guard = self.lock()?;
        if to.is_none() && self.tasks_since(list, since)? > 0 {
            bail!("в списке появились задачи — выберите, куда их перенести");
        }
        let moved = match to {
            Some(to) => self.move_list_tasks(list, to, since)?,
            None => 0,
        };
        let used = moved > 0 || self.list_used(list)?;
        let lists = self.edit_lists(|lists| if used { lists.archive(list) } else { lists.remove(list) })?;
        Ok((lists, moved, used))
    }

    pub fn lists_stamp(&self) -> Option<Stamp> {
        self.store.stamp(LISTS_FILE)
    }

    /// Переводит книжку прежних версий на нынешний формат — один раз, при запуске, а не при
    /// чтении: скрытые от ИИ списки из `AI_HIDDEN.md` — в `LISTS.md`. Возвращает, что сделано.
    pub fn migrate(&self) -> Result<Vec<String>> {
        let _guard = self.lock()?;
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
        let _guard = self.lock()?;
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

    /// Согласованный снимок записей и отпечатка для клиентского черновика.
    pub fn open_notes(&self, id: &str) -> Result<(Notes, Option<Stamp>)> {
        let _guard = self.lock()?;
        Ok((self.load_notes(id)?, self.notes_stamp(id)))
    }

    /// Сохраняет записи; пустые — без файла, как и пустая страница.
    pub fn save_notes(&self, id: &str, notes: &Notes) -> Result<()> {
        let _guard = self.lock()?;
        let text = if notes.is_empty() { String::new() } else { notes.serialize() };
        self.write_or_remove(&notes_path(id), &text)
    }

    /// Commit черновика записей; read error не разрешает затереть внешнюю версию.
    pub fn save_notes_draft(&self, id: &str, notes: &mut Notes, stamp: &mut Option<Stamp>) -> Result<()> {
        let _guard = self.lock()?;
        let mut candidate = notes.clone();
        if self.notes_stamp(id) != *stamp {
            candidate.merge(&self.load_notes(id)?);
        } else if stamp.is_none() {
            self.read_file(&notes_path(id))?;
        }
        self.save_notes(id, &candidate)?;
        *stamp = self.notes_stamp(id);
        *notes = candidate;
        Ok(())
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

    fn reserve_ids(&self, ids: impl IntoIterator<Item = String>) -> Result<()> {
        let key = self.identity_key()?;
        reserved_ids().lock().map_err(|_| anyhow::anyhow!("реестр id недоступен"))?.entry(key).or_default().extend(ids);
        Ok(())
    }

    fn identity_key(&self) -> Result<PathBuf> {
        if let Ok(path) = self.dir.canonicalize() {
            return Ok(path);
        }
        Ok(if self.dir.is_absolute() { self.dir.clone() } else { std::env::current_dir()?.join(&self.dir) })
    }

    /// 64 случайных бита, 16 lowercase hex; ошибка OS random не подменяется временем.
    /// Резервируются и ещё не сохранённые id этого процесса.
    pub fn new_id(&self) -> Result<String> {
        self.allocate_ids(fresh_id)
    }

    fn allocate_ids<T>(&self, allocate: impl FnOnce(&mut fs::File, &mut HashSet<String>) -> Result<T>) -> Result<T> {
        let _guard = self.lock()?;
        let mut occupied: HashSet<String> =
            self.lists()?.all().iter().map(|list| list.id.as_str().to_owned()).collect();
        for date in self.dates()? {
            let board = self.load(date)?;
            occupied.extend(board.tasks().filter_map(|task| task.id.clone()));
            occupied.extend(board.graves().iter().map(|(id, _)| id.clone()));
        }
        for path in self.files()? {
            if let Some(FileKind::Notes(id)) = file_kind(&path) {
                occupied.insert(id);
            }
        }
        let key = self.identity_key()?;
        let mut reserved = reserved_ids().lock().map_err(|_| anyhow::anyhow!("реестр id недоступен"))?;
        let reserved = reserved.entry(key).or_default();
        reserved.extend(occupied);
        let mut random = fs::File::open("/dev/urandom").context("системный источник случайности")?;
        allocate(&mut random, reserved)
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
        let _guard = self.lock()?;
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
        let _guard = self.lock()?;
        let mut text = self.read_text(DELETED_NOTES)?.unwrap_or_default();
        text.push_str(&format!("## {title}\n<!-- id:{id}, страница {page} -->\n\n{body}\n"));
        self.store.write(DELETED_NOTES, text.as_bytes())
    }

    /// Автоперенос в начале дня: невыполненные задачи с последней заполненной страницы до
    /// `today` переезжают в конец тех же списков на `today`, а там помечаются `[>]`.
    /// Возвращает дату, с которой перенесли, и сколько задач.
    pub fn carry_over(&self, today: Date) -> Result<Option<(Date, usize)>> {
        let _guard = self.lock()?;
        let Some(from) = self.dates()?.into_iter().rev().find(|&d| d < today) else {
            return Ok(None);
        };
        let mut source = self.load(from)?;
        // Копии на сегодня — с теми же id, что и на исходной странице: если перенос сделают
        // два устройства сразу, при слиянии это будут одни и те же задачи, а не дубли.
        if self.assign_ids(&mut source)? {
            // Закрепляем идентичности до первой копии, не помечая источник перенесённым.
            self.save(from, &source)?;
        }
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
            upsert_carried(self, &mut target, category, task, from)?;
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
        let _guard = self.lock()?;
        if from == to || board.list(category).get(index).is_none_or(|task| !task.is_open()) {
            return Ok(false);
        }
        self.assign_ids(board)?;
        // Сохраняем устойчивый id и локальные правки прежде целевой копии.
        self.save(from, board)?;
        let task = board.list(category)[index].clone();
        let mut target = self.load(to)?;
        upsert_carried(self, &mut target, category, task.carried(), from)?;
        self.save(to, &target)?;
        let mut source = board.clone();
        let task = source.get_mut(category, index).unwrap();
        task.state = State::Migrated(to);
        task.touch();
        self.save(from, &source)?;
        *board = source;
        Ok(true)
    }
}

fn upsert_carried(book: &Book, target: &mut Board, category: ListId, task: Task, from: Date) -> Result<()> {
    if task.id.as_deref().is_some_and(|id| target.buried(id).is_some()) {
        return Ok(());
    }
    for existing_category in target.ids() {
        if let Some(index) = target.list(existing_category).iter().position(|existing| existing.id == task.id) {
            // Восстановить можно исторического предка источника, но не цель, которую
            // уже выполнили, отменили или после частичного переноса отправили дальше.
            if let State::Migrated(next) = target.list(existing_category)[index].state
                && migration_reaches(book, task.id.as_deref().unwrap(), next, from)?
            {
                target.remove(existing_category, index);
                target.push(category, task);
            }
            return Ok(());
        }
    }
    target.push(category, task);
    Ok(())
}

fn migration_reaches(book: &Book, id: &str, mut date: Date, from: Date) -> Result<bool> {
    let mut visited = HashSet::new();
    while visited.insert(date) {
        if date == from {
            return Ok(true);
        }
        let board = book.load(date)?;
        match board.tasks().find(|task| task.id.as_deref() == Some(id)).map(|task| task.state) {
            Some(State::Migrated(next)) => date = next,
            _ => return Ok(false),
        }
    }
    Ok(false)
}

fn reserved_ids() -> &'static Mutex<HashMap<PathBuf, HashSet<String>>> {
    static IDS: LazyLock<Mutex<HashMap<PathBuf, HashSet<String>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
    &IDS
}

fn fresh_id(random: &mut impl Read, reserved: &mut HashSet<String>) -> Result<String> {
    loop {
        let mut bytes = [0u8; 8];
        random.read_exact(&mut bytes).context("случайный id")?;
        let id = format!("{:016x}", u64::from_le_bytes(bytes));
        if reserved.insert(id.clone()) {
            return Ok(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    #[test]
    fn historical_return_follows_multiple_dates_but_retry_does_not_undo_a_later_move() {
        let (book, faults) = fault_book("migration-chain");
        let mut first = Board::default();
        first.push(ListId::WORK, Task::new("task"));
        book.save(d(25), &first).unwrap();
        book.postpone(&mut first, d(25), ListId::WORK, 0, d(26)).unwrap();
        let mut second = book.load(d(26)).unwrap();
        book.postpone(&mut second, d(26), ListId::WORK, 0, d(27)).unwrap();
        let mut third = book.load(d(27)).unwrap();
        book.postpone(&mut third, d(27), ListId::WORK, 0, d(25)).unwrap();
        assert!(book.load(d(25)).unwrap().list(ListId::WORK)[0].is_open());

        // Новый перенос: цель успели передвинуть дальше до повторной фиксации источника.
        first = book.load(d(25)).unwrap();
        faults.fail_at.set(Some(faults.writes.get() + 3));
        assert!(book.postpone(&mut first, d(25), ListId::WORK, 0, d(28)).is_err());
        faults.fail_at.set(None);
        let mut target = book.load(d(28)).unwrap();
        book.postpone(&mut target, d(28), ListId::WORK, 0, d(29)).unwrap();
        book.postpone(&mut first, d(25), ListId::WORK, 0, d(28)).unwrap();
        assert_eq!(book.load(d(28)).unwrap().list(ListId::WORK)[0].state, State::Migrated(d(29)));
        assert!(book.load(d(29)).unwrap().list(ListId::WORK)[0].is_open());
    }

    #[derive(Default)]
    struct Faults {
        writes: Cell<usize>,
        fail_at: Cell<Option<usize>>,
        fail_read: Cell<bool>,
    }

    struct FaultStore {
        inner: crate::store::MemStore,
        faults: Rc<Faults>,
    }

    impl Store for FaultStore {
        fn read(&self, path: &str) -> Result<Option<Vec<u8>>> {
            if self.faults.fail_read.get() {
                bail!("injected read failure");
            }
            self.inner.read(path)
        }
        fn write(&self, path: &str, data: &[u8]) -> Result<()> {
            let write = self.faults.writes.get() + 1;
            self.faults.writes.set(write);
            if self.faults.fail_at.get() == Some(write) {
                bail!("injected write failure");
            }
            self.inner.write(path, data)
        }
        fn remove(&self, path: &str) -> Result<()> {
            self.inner.remove(path)
        }
        fn list(&self, dir: &str) -> Result<Vec<String>> {
            self.inner.list(dir)
        }
        fn stamp(&self, path: &str) -> Option<Stamp> {
            if self.faults.fail_read.get() { None } else { self.inner.stamp(path) }
        }
    }

    fn fault_book(name: &str) -> (Book, Rc<Faults>) {
        let faults = Rc::new(Faults::default());
        let store = FaultStore { inner: crate::store::MemStore::default(), faults: faults.clone() };
        (Book::with_store(PathBuf::from(format!("memory-fault-{name}")), Box::new(store)), faults)
    }

    #[test]
    fn stale_snapshots_conflict_instead_of_losing_writes() {
        let book = temp_book("stale");
        let mut first = book.load(d(26)).unwrap();
        let mut second = book.load(d(26)).unwrap();
        first.push(ListId::WORK, Task::new("first"));
        second.push(ListId::WORK, Task::new("second"));
        book.save(d(26), &first).unwrap();
        assert!(book.save(d(26), &second).is_err());
        assert_eq!(texts(&book.load(d(26)).unwrap(), ListId::WORK), [("first".into(), State::Open)]);
        fs::remove_dir_all(book.dir()).unwrap();
    }

    #[test]
    fn simultaneous_draft_commits_keep_both_edits() {
        use std::sync::{Arc, Barrier};
        let book = temp_book("concurrent");
        let dir = book.dir().to_owned();
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = ["first", "second"]
            .into_iter()
            .map(|text| {
                let dir = dir.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let book = Book::new(dir);
                    let (mut board, mut stamp) = book.open_page(d(26)).unwrap();
                    board.push(ListId::WORK, Task::new(text));
                    barrier.wait();
                    book.save_page(d(26), &mut board, &mut stamp).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let board = book.load(d(26)).unwrap();
        let mut result: Vec<_> = board.tasks().map(|task| task.text.as_str()).collect();
        result.sort();
        assert_eq!(result, ["first", "second"]);
        assert!(board.tasks().all(|task| task.id.as_ref().is_some_and(|id| id.len() == 16)));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn nested_mutators_share_one_lock_and_serialized_rmw() {
        use std::sync::{Arc, Barrier};
        let book = temp_book("rmw");
        let dir = book.dir().to_owned();
        let barrier = Arc::new(Barrier::new(4));
        let threads: Vec<_> = (0..4)
            .map(|writer| {
                let dir = dir.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let book = Book::new(dir);
                    barrier.wait();
                    for index in 0..8 {
                        let _outer = book.lock().unwrap();
                        let _inner = book.lock().unwrap();
                        let mut board = book.load(d(26)).unwrap();
                        board.push(ListId::WORK, Task::new(format!("{writer}/{index}")));
                        book.save(d(26), &board).unwrap();
                        book.archive_entry("task", "abc", d(26), "stamp", "entry").unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(book.load(d(26)).unwrap().tasks().count(), 32);
        let archive = String::from_utf8(book.read_file(DELETED_NOTES).unwrap().unwrap()).unwrap();
        assert_eq!(archive.matches("## stamp").count(), 32);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_draft_commit_keeps_memory_and_confirmed_stamp() {
        let (book, faults) = fault_book("draft");
        let (mut board, mut stamp) = book.open_page(d(26)).unwrap();
        board.push(ListId::WORK, Task::new("draft"));
        let original = board.clone();
        faults.fail_at.set(Some(1));
        assert!(book.save_page(d(26), &mut board, &mut stamp).is_err());
        assert_eq!(board, original);
        assert!(board.tasks().all(|task| task.id.is_none()));
        assert_eq!(stamp, None);
        faults.fail_at.set(None);
        book.save_page(d(26), &mut board, &mut stamp).unwrap();
        assert_eq!(book.load(d(26)).unwrap(), board);
    }

    #[test]
    fn outside_corruption_preserves_file_draft_and_stamp() {
        let book = temp_book("corruption");
        let (mut board, mut stamp) = book.open_page(d(26)).unwrap();
        board.push(ListId::WORK, Task::new("original"));
        book.save_page(d(26), &mut board, &mut stamp).unwrap();
        board.push(ListId::HOME, Task::new("local draft"));
        let original = board.clone();
        let confirmed = stamp;
        for raw in [b"USER TEXT NOT VALID AS A PAGE".as_slice(), b"\xff\xfe".as_slice()] {
            crate::store::write_atomic(&book.dir().join(page_path(d(26))), raw).unwrap();
            assert!(book.save_page(d(26), &mut board, &mut stamp).is_err());
            assert_eq!(fs::read(book.dir().join(page_path(d(26)))).unwrap(), raw);
            assert_eq!(board, original);
            assert_eq!(stamp, confirmed);
        }
        fs::remove_file(book.dir().join(page_path(d(26)))).unwrap();
        fs::create_dir(book.dir().join(page_path(d(26)))).unwrap();
        assert!(book.save_page(d(26), &mut board, &mut stamp).is_err());
        assert_eq!(board, original);
        assert_eq!(stamp, confirmed);
        fs::remove_dir_all(book.dir()).unwrap();
    }

    #[test]
    fn absent_stamp_does_not_hide_read_error() {
        let (book, faults) = fault_book("read");
        let (mut board, mut stamp) = book.open_page(d(26)).unwrap();
        board.push(ListId::WORK, Task::new("draft"));
        faults.fail_read.set(true);
        assert!(book.load(d(26)).is_err());
        assert!(book.save_page(d(26), &mut board, &mut stamp).is_err());
        assert_eq!(stamp, None);
        assert_eq!(faults.writes.get(), 0);
        assert_eq!(board.tasks().next().unwrap().text, "draft");
    }

    #[test]
    fn final_task_grave_survives_restart_and_suppresses_stale_copy() {
        let book = temp_book("grave");
        let mut board = Board::default();
        let mut task = Task::new("deleted");
        task.id = Some("abc123".into());
        board.push(ListId::WORK, task);
        book.save(d(26), &board).unwrap();
        let stale = book.load(d(26)).unwrap();
        board.delete(ListId::WORK, 0).unwrap();
        book.save(d(26), &board).unwrap();
        assert!(book.page_stamp(d(26)).is_some());
        let restarted = Book::new(book.dir().to_owned());
        let empty = restarted.load(d(26)).unwrap();
        assert!(empty.is_empty());
        assert!(empty.buried("abc123").is_some());
        assert!(crate::merge::merge_boards(&empty, &stale).is_empty());
        let mut remote = stale;
        remote.push(ListId::HOME, Task::new("new task"));
        let merged = crate::merge::merge_boards(&empty, &remote);
        assert_eq!(merged.tasks().count(), 1);
        assert_eq!(merged.tasks().next().unwrap().text, "new task");
        fs::remove_dir_all(book.dir()).unwrap();
    }

    #[test]
    fn carry_retry_is_idempotent_at_every_write_including_missing_source_id() {
        for with_id in [false, true] {
            let writes = if with_id { 2 } else { 3 };
            for step in 1..=writes {
                let (book, faults) = fault_book(&format!("carry-{with_id}-{step}"));
                let mut source = Board::default();
                let mut task = Task::new("move me");
                if with_id {
                    task.id = Some("manual1".into());
                }
                source.push(ListId::WORK, task);
                book.save(d(25), &source).unwrap();
                faults.fail_at.set(Some(faults.writes.get() + step));
                assert!(book.carry_over(d(26)).is_err());
                faults.fail_at.set(None);
                let mut target = book.load(d(26)).unwrap();
                if let Some(task) = target.get_mut(ListId::WORK, 0) {
                    task.state = State::Done;
                    book.save(d(26), &target).unwrap();
                }
                let completed = !target.is_empty();
                book.carry_over(d(26)).unwrap();
                let target = book.load(d(26)).unwrap();
                assert_eq!(target.tasks().count(), 1, "id={with_id} step={step}");
                assert_eq!(target.list(ListId::WORK)[0].state, if completed { State::Done } else { State::Open });
                let source = book.load(d(25)).unwrap();
                assert_eq!(source.list(ListId::WORK)[0].id, target.list(ListId::WORK)[0].id);
                assert_eq!(source.list(ListId::WORK)[0].state, State::Migrated(d(26)));
                assert_eq!(book.carry_over(d(26)).unwrap(), None);
            }
        }
    }

    #[test]
    fn postpone_retry_is_idempotent_and_keeps_notes_and_completed_target() {
        for with_id in [false, true] {
            for step in 1..=3 {
                let (book, faults) = fault_book(&format!("postpone-{with_id}-{step}"));
                let mut board = Board::default();
                let mut task = Task::new("move me");
                if with_id {
                    task.id = Some("manual1".into());
                }
                board.push(ListId::WORK, task);
                book.save(d(25), &board).unwrap();
                faults.fail_at.set(Some(faults.writes.get() + step));
                assert!(book.postpone(&mut board, d(25), ListId::WORK, 0, d(26)).is_err());
                assert!(board.list(ListId::WORK)[0].is_open());
                faults.fail_at.set(None);
                // Перезапуск: повтор читает закреплённый источник, а не старый черновик.
                let mut source = book.load(d(25)).unwrap();
                let mut target = book.load(d(26)).unwrap();
                let completed = !target.is_empty();
                if let Some(task) = target.get_mut(ListId::WORK, 0) {
                    task.state = State::Done;
                    let id = task.id.clone().unwrap();
                    book.save(d(26), &target).unwrap();
                    let mut notes = Notes::default();
                    notes.add_subtask("kept");
                    book.save_notes(&id, &notes).unwrap();
                }
                book.postpone(&mut source, d(25), ListId::WORK, 0, d(26)).unwrap();
                let target = book.load(d(26)).unwrap();
                assert_eq!(target.tasks().count(), 1, "id={with_id} step={step}");
                let task = &target.list(ListId::WORK)[0];
                assert_eq!(task.state, if completed { State::Done } else { State::Open });
                assert_eq!(source.list(ListId::WORK)[0].id, task.id);
                if completed {
                    assert_eq!(book.load_notes(task.id.as_deref().unwrap()).unwrap().visible().len(), 1);
                }
            }
        }
    }

    #[test]
    fn identities_reserve_tasks_lists_graves_and_unsaved_values() {
        let (book, _) = fault_book("reserved");
        let mut board = Board::default();
        let mut task = Task::new("manual");
        task.id = Some("0123456789abcdef".into());
        board.push(ListId::WORK, task);
        board.bury("1111111111111111", 1);
        book.save(d(26), &board).unwrap();
        let mut lists = book.lists().unwrap();
        lists.add(ListId::new("2222222222222222"), "new").unwrap();
        book.save_lists(&lists).unwrap();
        let first = book.new_id().unwrap();
        let second = book.new_id().unwrap();
        assert_ne!(first, second);
        assert_eq!(first.len(), 16);
        assert!(first.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        let key = book.identity_key().unwrap();
        let reserved = reserved_ids().lock().unwrap();
        let reserved = &reserved[&key];
        for id in ["0123456789abcdef", "1111111111111111", "2222222222222222", &first, &second] {
            assert!(reserved.contains(id), "{id}");
        }
        let mut random = std::io::Cursor::new([1u64.to_le_bytes(), 2u64.to_le_bytes()].concat());
        let mut reserved = HashSet::from(["0000000000000001".to_owned()]);
        assert_eq!(fresh_id(&mut random, &mut reserved).unwrap(), "0000000000000002");
        assert!(fresh_id(&mut std::io::empty(), &mut reserved).is_err());
    }

    use super::*;

    #[test]
    fn postponing_back_to_a_historical_date_restores_one_live_copy() {
        let (book, _) = fault_book("return");
        let mut source = Board::default();
        source.push(ListId::WORK, Task::new("there and back"));
        book.save(d(25), &source).unwrap();
        book.postpone(&mut source, d(25), ListId::WORK, 0, d(26)).unwrap();
        let id = source.list(ListId::WORK)[0].id.clone();
        let mut later = book.load(d(26)).unwrap();
        later.get_mut(ListId::WORK, 0).unwrap().text = "updated on later date".into();
        book.postpone(&mut later, d(26), ListId::WORK, 0, d(25)).unwrap();
        let earlier = book.load(d(25)).unwrap();
        assert_eq!(earlier.tasks().count(), 1);
        assert_eq!(earlier.list(ListId::WORK)[0].id, id);
        assert_eq!(earlier.list(ListId::WORK)[0].state, State::Open);
        assert_eq!(earlier.list(ListId::WORK)[0].text, "updated on later date");
        assert_eq!(later.list(ListId::WORK)[0].state, State::Migrated(d(25)));
    }

    #[test]
    fn retry_does_not_reopen_or_overwrite_an_existing_live_target() {
        for state in [State::Open, State::Done, State::Cancelled] {
            let (book, faults) = fault_book(&format!("target-{state:?}"));
            let mut source = Board::default();
            source.push(ListId::WORK, Task::new("original"));
            book.save(d(25), &source).unwrap();
            faults.fail_at.set(Some(faults.writes.get() + 3));
            assert!(book.postpone(&mut source, d(25), ListId::WORK, 0, d(26)).is_err());
            faults.fail_at.set(None);
            let mut target = book.load(d(26)).unwrap();
            let task = target.get_mut(ListId::WORK, 0).unwrap();
            task.state = state;
            task.text = "changed after the partial transfer".into();
            task.touch();
            book.save(d(26), &target).unwrap();
            book.postpone(&mut source, d(25), ListId::WORK, 0, d(26)).unwrap();
            let target = book.load(d(26)).unwrap();
            assert_eq!(target.tasks().count(), 1);
            assert_eq!(target.list(ListId::WORK)[0].state, state);
            assert_eq!(target.list(ListId::WORK)[0].text, "changed after the partial transfer");
        }
    }

    #[test]
    fn interprocess_rmw_uses_the_same_book_lock() {
        const CHILD: &str = "ZTASKMGR_BOOK_TEST_CHILD";
        if let Some(dir) = std::env::var_os(CHILD) {
            let book = Book::new(PathBuf::from(dir));
            for index in 0..6 {
                let _guard = book.lock().unwrap();
                let mut board = book.load(d(26)).unwrap();
                board.push(ListId::WORK, Task::new(format!("{}-{index}", std::process::id())));
                book.save(d(26), &board).unwrap();
            }
            return;
        }
        let book = temp_book("process-rmw");
        let children: Vec<_> = (0..3)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "book::tests::interprocess_rmw_uses_the_same_book_lock"])
                    .env(CHILD, book.dir())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        }
        let board = book.load(d(26)).unwrap();
        assert_eq!(board.tasks().count(), 18);
        assert_eq!(board.tasks().map(|task| &task.text).collect::<HashSet<_>>().len(), 18);
        fs::remove_dir_all(book.dir()).unwrap();
    }

    #[test]
    fn duplicate_ids_do_not_get_silently_merged_and_date_copies_are_legal() {
        let (book, _) = fault_book("duplicates");
        let text = "## Работа\n- [ ] first <!-- id:same -->\n";
        book.write_file("2026-10-25.md", text.as_bytes()).unwrap();
        book.write_file("2026-10-26.md", text.as_bytes()).unwrap();
        assert_eq!(book.history("same").unwrap().len(), 2);
        let invalid = "## Работа\n- [ ] first <!-- id:same -->\n## Дом\n- [ ] unrelated <!-- id:same -->\n";
        assert!(book.merge_text("2026-10-26.md", invalid).is_err());
        assert_eq!(book.read_file("2026-10-26.md").unwrap().unwrap(), text.as_bytes());
        book.write_file("2026-10-26.md", invalid.as_bytes()).unwrap();
        assert!(book.load(d(26)).is_err());
        assert!(book.new_id().is_err(), "identity allocator cannot ignore a corrupt page");
    }

    #[test]
    fn notes_read_failure_preserves_pending_draft_and_confirmed_stamp() {
        let (book, faults) = fault_book("notes-draft");
        let mut notes = Notes::default();
        notes.add_subtask("local");
        let mut stamp = None;
        faults.fail_read.set(true);
        assert!(book.save_notes_draft("abc", &mut notes, &mut stamp).is_err());
        assert_eq!(stamp, None);
        assert_eq!(notes.visible().len(), 1);
        assert_eq!(faults.writes.get(), 0);
        faults.fail_read.set(false);
        book.save_notes_draft("abc", &mut notes, &mut stamp).unwrap();
        assert_eq!(book.load_notes("abc").unwrap(), notes);
    }

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
        let id = book.new_id().unwrap();
        assert_eq!(id.len(), 16);
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
