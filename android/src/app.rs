//! Что делает приложение: страница дня, задача, её записи, настройки. Экраны — в
//! `ui/app.slint`, книжка — в `ztaskmgr-core`; здесь между ними: правки идут в книжку, а
//! книжка — на экран (`Ctx::show`).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
use ztaskmgr_core::book::{Book, Stamp};
use ztaskmgr_core::clock::{self, Date};
use ztaskmgr_core::config::{self, Config};
use ztaskmgr_core::lists::Lists;
use ztaskmgr_core::model::{ListId, State, Task};
use ztaskmgr_core::notes::{Kind, Notes};
use ztaskmgr_core::page::Page;
use ztaskmgr_core::webdav::{self, Worker};

slint::include_modules!();

/// Через сколько после правки синхронизироваться: правки подряд уходят одной.
const SYNC_DELAY: Duration = Duration::from_secs(3);
/// Повтор системной «Назад» в пределах этого времени — то же нажатие.
const BACK_REPEAT: Duration = Duration::from_millis(500);
/// Сколько держится сообщение в строке состояния.
const STATUS_FOR: Duration = Duration::from_secs(8);

const WEEKDAYS: [&str; 7] = ["Пн", "Вт", "Ср", "Чт", "Пт", "Сб", "Вс"];
const MONTHS: [&str; 12] = [
    "января",
    "февраля",
    "марта",
    "апреля",
    "мая",
    "июня",
    "июля",
    "августа",
    "сентября",
    "октября",
    "ноября",
    "декабря",
];
const MONTHS_SHORT: [&str; 12] = ["янв", "фев", "мар", "апр", "мая", "июн", "июл", "авг", "сен", "окт", "ноя", "дек"];

// Дни и месяцы — своей таблицей: в Android нет системной локали, `strftime` знает только «C».

/// «Ср, 28 октября» (и год, если не текущий).
fn day_title(date: Date, today: Date) -> String {
    let mut title =
        format!("{}, {} {}", WEEKDAYS[date.weekday() as usize], date.day(), MONTHS[date.month() as usize - 1]);
    if date.year() != today.year() {
        title.push_str(&format!(" {}", date.year()));
    }
    title
}

/// «30 окт».
fn short_date(date: Date) -> String {
    format!("{} {}", date.day(), MONTHS_SHORT[date.month() as usize - 1])
}

/// «30 октября».
fn long_date(date: Date) -> String {
    format!("{} {}", date.day(), MONTHS[date.month() as usize - 1])
}

fn state_code(state: State) -> i32 {
    match state {
        State::Open => 0,
        State::Done => 1,
        State::Cancelled => 2,
        State::Migrated(_) => 3,
    }
}

/// Открытые записи задачи.
struct NotesView {
    id: String,
    notes: Notes,
    stamp: Option<Stamp>,
    /// Пункты на экране — номера в `notes`.
    shown: Vec<usize>,
}

pub struct Ctx {
    book: Book,
    config: Config,
    today: Date,
    registry: Lists,
    lists_stamp: Option<Stamp>,
    page: Page,
    /// Страница не прочитана (ошибка в файле) — только смотреть.
    page_error: bool,
    /// Списки на экране, по порядку вкладок.
    shown: Vec<ListId>,
    active: ListId,
    /// Открытая задача — по id: номер в списке меняется, когда задачи переезжают.
    task: Option<String>,
    notes: Option<NotesView>,
    worker: Worker,
    edited_at: Option<Instant>,
    status: Option<(String, Instant)>,
    sync: String,
    /// Когда последний раз сработала системная «Назад».
    last_back: Option<Instant>,
}

impl Ctx {
    pub fn new(dir: PathBuf, config_path: PathBuf) -> Self {
        let book = Book::new(dir);
        let mut status = None;
        let mut say = |s: String| status = Some((s, Instant::now()));
        if let Err(e) = book.migrate() {
            say(format!("Книжка: {e:#}"));
        }
        let config = Config::load(config_path).unwrap_or_else(|e| {
            say(format!("Настройки: {e:#}"));
            Config::default()
        });
        let today = Date::today();
        match book.carry_over(today) {
            Ok(Some((from, count))) => say(format!("Перенесено с {}: {count}", long_date(from))),
            Ok(None) => {}
            Err(e) => say(format!("Автоперенос: {e:#}")),
        }
        let settings = config.webdav().map(webdav::Settings::resolve).transpose().unwrap_or_else(|e| {
            say(format!("WebDAV: {e:#}"));
            None
        });
        let worker = Worker::start(book.dir(), settings);
        let page = Page::open(&book, today).unwrap_or_else(|_| Page::empty(today));
        let mut ctx = Self {
            book,
            config,
            today,
            registry: Lists::default(),
            lists_stamp: None,
            page,
            page_error: false,
            shown: Vec::new(),
            active: ListId::OTHER,
            task: None,
            notes: None,
            worker,
            edited_at: None,
            status,
            sync: String::new(),
            last_back: None,
        };
        ctx.load_registry();
        ctx.open_page(today);
        if let Some(first) = ctx.shown.first() {
            ctx.active = *first;
        }
        ctx
    }

    fn say(&mut self, text: impl Into<String>) {
        self.status = Some((text.into(), Instant::now()));
    }

    fn load_registry(&mut self) {
        match self.book.lists() {
            Ok(lists) => self.registry = lists,
            Err(e) => self.say(format!("Списки: {e:#}")),
        }
        self.lists_stamp = self.book.lists_stamp();
        self.refresh_shown();
    }

    fn refresh_shown(&mut self) {
        self.shown = self.registry.order(self.page.board.ids());
        if self.shown.is_empty() {
            self.shown.push(ListId::OTHER);
        }
        if !self.shown.contains(&self.active) {
            self.active = self.shown[0];
        }
    }

    fn list_title(&self, id: ListId) -> String {
        self.registry.title(id).or(self.page.board.title(id)).unwrap_or(id.as_str()).to_string()
    }

    fn open_page(&mut self, date: Date) {
        match Page::open(&self.book, date) {
            Ok(page) => {
                self.page = page;
                self.page_error = false;
            }
            Err(e) => {
                self.page = Page::empty(date);
                self.page_error = true;
                self.say(format!("Страница не прочитана, только просмотр: {e:#}"));
            }
        }
        self.task = None;
        self.notes = None;
        self.refresh_shown();
    }

    fn save(&mut self) {
        if self.page_error {
            return;
        }
        if let Err(e) = self.page.save(&self.book) {
            self.say(format!("Ошибка сохранения: {e:#}"));
        }
        self.edited();
    }

    fn edited(&mut self) {
        self.edited_at = Some(Instant::now());
    }

    /// Где сейчас открытая задача.
    fn task_pos(&self) -> Option<(ListId, usize)> {
        let id = self.task.as_deref()?;
        self.page.board.ids().into_iter().find_map(|c| {
            let i = self.page.board.list(c).iter().position(|t| t.id.as_deref() == Some(id))?;
            Some((c, i))
        })
    }

    fn open_task(&mut self, index: usize) -> bool {
        let Some(task) = self.page.board.list(self.active).get(index) else {
            return false;
        };
        // Задача, написанная руками, получает id — по нему её и держим.
        if task.id.is_none() {
            self.save();
        }
        self.task = self.page.board.list(self.active).get(index).and_then(|t| t.id.clone());
        self.task.is_some()
    }

    fn toggle(&mut self, category: ListId, index: usize, cancel: bool) {
        let Some(task) = self.page.board.list(category).get(index) else {
            return;
        };
        let state = match (task.state, cancel) {
            (State::Migrated(to), _) => {
                return self.say(format!("Задача перенесена на {} — отметьте её там", long_date(to)));
            }
            // Отменённую всё-таки сделали — выполнена.
            (State::Open | State::Cancelled, false) => State::Done,
            (State::Done, false) => State::Open,
            (State::Open | State::Done, true) => State::Cancelled,
            (State::Cancelled, true) => State::Open,
        };
        self.page.board.set_state(category, index, state);
        self.save();
    }

    fn add_task(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() || self.page_error {
            return;
        }
        let mut task = Task::new(text);
        task.touch();
        self.page.board.push(self.active, task);
        self.save();
        self.refresh_shown();
    }

    fn postpone(&mut self, text: &str) -> bool {
        let Some((category, index)) = self.task_pos() else {
            return false;
        };
        let to = match text.trim() {
            "tomorrow" => self.page.date.max(self.today).add_days(1),
            other => match Date::parse_input(other, self.page.date) {
                Some(date) => date,
                None => {
                    self.say(format!("Не понял дату «{other}»: 30.10, 30.10.2026 или +3"));
                    return false;
                }
            },
        };
        if to == self.page.date {
            self.say("Это та же страница");
            return false;
        }
        match self.book.postpone(&mut self.page.board, self.page.date, category, index, to) {
            Ok(true) => {
                self.page.saved_by_book(&self.book);
                self.edited();
                self.say(format!("Перенесено на {}", long_date(to)));
                true
            }
            Ok(false) => {
                self.say("Переносятся только невыполненные задачи");
                false
            }
            Err(e) => {
                self.say(format!("Перенос: {e:#}"));
                false
            }
        }
    }

    fn move_to_list(&mut self, to: ListId) {
        let Some((from, index)) = self.task_pos() else {
            return;
        };
        if let Some(new_index) = self.page.board.move_to(from, index, to) {
            // Из скрытого от ИИ списка в открытый задача уходит скрытой.
            if self.registry.is_hidden(from) && !self.registry.is_hidden(to) {
                self.page.board.get_mut(to, new_index).unwrap().private = true;
            }
            self.active = to;
            self.save();
            self.refresh_shown();
        }
    }

    fn delete_task(&mut self) {
        let Some((category, index)) = self.task_pos() else {
            return;
        };
        let task = self.page.board.delete(category, index);
        self.save();
        if let Some(task) = task {
            match self.book.archive_notes(&task, self.page.date, &clock::stamp()) {
                Ok(true) => self.say("Записи задачи сохранены в DELETED_NOTES.md"),
                Ok(false) => {}
                Err(e) => self.say(format!("Записи задачи: {e:#}")),
            }
        }
        self.task = None;
    }

    fn task_history(&self, id: &str) -> String {
        let Ok(history) = self.book.history(id) else {
            return String::new();
        };
        if history.len() < 2 {
            return String::new();
        }
        let lines: Vec<String> = history
            .iter()
            .map(|(date, list, state)| {
                let what = match state {
                    State::Open => "открыта".to_string(),
                    State::Done => "выполнена".to_string(),
                    State::Cancelled => "отменена".to_string(),
                    State::Migrated(to) => format!("перенесена на {}", short_date(*to)),
                };
                format!("{} · {} · {what}", short_date(*date), self.list_title(*list))
            })
            .collect();
        format!("История:\n{}", lines.join("\n"))
    }

    // ------------------------------------------------------------------ записи

    fn open_notes(&mut self) -> bool {
        let Some(id) = self.task.clone() else {
            return false;
        };
        match self.book.load_notes(&id) {
            Ok(notes) => {
                let stamp = self.book.notes_stamp(&id);
                self.notes = Some(NotesView { shown: notes.visible(), id, notes, stamp });
                true
            }
            Err(e) => {
                self.say(format!("Записи задачи: {e:#}"));
                false
            }
        }
    }

    fn save_notes(&mut self) {
        let Some(view) = &mut self.notes else {
            return;
        };
        // Записи поменяли снаружи (синхронизация) — сливаем, а не затираем.
        if self.book.notes_stamp(&view.id) != view.stamp
            && let Ok(disk) = self.book.load_notes(&view.id)
        {
            view.notes.merge(&disk);
        }
        let result = self.book.save_notes(&view.id, &view.notes);
        view.stamp = self.book.notes_stamp(&view.id);
        view.shown = view.notes.visible();
        if let Err(e) = result {
            self.say(format!("Записи задачи: {e:#}"));
        }
        self.edited();
    }

    fn add_note(&mut self, text: &str, subtask: bool) {
        let text = text.trim_end();
        let Some(view) = &mut self.notes else {
            return;
        };
        if text.trim().is_empty() {
            return;
        }
        if subtask {
            view.notes.add_subtask(text.trim());
        } else {
            view.notes.add_note(&clock::stamp(), text);
        }
        self.save_notes();
    }

    fn toggle_note(&mut self, row: usize) {
        let Some(view) = &mut self.notes else {
            return;
        };
        if let Some(&i) = view.shown.get(row) {
            view.notes.toggle(i);
            self.save_notes();
        }
    }

    fn delete_note(&mut self, row: usize) {
        let task = self.task_pos().map(|(c, i)| self.page.board.list(c)[i].text.clone()).unwrap_or_default();
        let Some(view) = &mut self.notes else {
            return;
        };
        let Some(&i) = view.shown.get(row) else {
            return;
        };
        let raw = view.notes.raw(i);
        if let Err(e) = self.book.archive_entry(&task, &view.id, self.page.date, &clock::stamp(), &raw) {
            return self.say(format!("Архив записей: {e:#}"));
        }
        view.notes.remove(i);
        self.save_notes();
    }

    // ------------------------------------------------------------------ синхронизация

    fn reconfigure(&mut self) {
        match self.config.webdav().map(webdav::Settings::resolve).transpose() {
            Ok(settings) => {
                if settings.is_none() {
                    self.sync.clear();
                }
                self.worker.configure(settings);
            }
            Err(e) => {
                self.say(format!("WebDAV: {e:#}"));
                self.worker.configure(None);
            }
        }
    }

    fn on_sync(&mut self, outcome: webdav::Outcome) {
        match outcome {
            Ok(summary) => {
                if summary.downloaded + summary.merged > 0 {
                    match self.book.carry_over(self.today) {
                        Ok(Some((from, count))) => {
                            self.say(format!("Перенесено с {}: {count}", long_date(from)));
                            self.edited();
                        }
                        Ok(None) => {}
                        Err(e) => self.say(format!("Автоперенос: {e:#}")),
                    }
                }
                if summary.failed.is_empty() {
                    self.sync = format!("⇅ {}", clock::hm());
                } else {
                    self.sync = "⇅ ✗".into();
                    self.say(format!("WebDAV: не синхронизированы: {}", summary.failed.join("; ")));
                }
            }
            Err(e) => {
                if self.sync != "⇅ ✗" {
                    self.say(format!("WebDAV: {e}"));
                }
                self.sync = "⇅ ✗".into();
            }
        }
    }

    /// Пришедшее снаружи (синхронизация) — на экран.
    fn check_external(&mut self) {
        if self.book.lists_stamp() != self.lists_stamp {
            self.load_registry();
        }
        if !self.page_error && self.page.changed_outside(&self.book) {
            if let Err(e) = self.page.reload(&self.book) {
                self.say(format!("Страница: {e:#}"));
            }
            self.refresh_shown();
        }
        if let Some(view) = &mut self.notes
            && self.book.notes_stamp(&view.id) != view.stamp
            && let Ok(disk) = self.book.load_notes(&view.id)
        {
            view.notes = disk;
            view.stamp = self.book.notes_stamp(&view.id);
            view.shown = view.notes.visible();
        }
    }

    /// Раз в секунду: синхронизация после правки, её итоги, новый день, старое сообщение.
    /// `true` — что-то поменялось, экран надо обновить.
    fn tick(&mut self) -> bool {
        let mut changed = false;
        if self.edited_at.is_some_and(|t| t.elapsed() >= SYNC_DELAY) {
            self.edited_at = None;
            self.worker.now();
        }
        for outcome in self.worker.outcomes() {
            self.on_sync(outcome);
            changed = true;
        }
        if let Some(result) = self.worker.passphrase_changed() {
            match result {
                Ok(new) => match self.config.set(config::WEBDAV_PASSPHRASE, &new) {
                    Ok(()) => self.say("Фраза сменена — введите новую фразу на других устройствах"),
                    Err(e) => self.say(format!("Фраза сменена на сервере, но не сохранена ({e:#}) — впишите её")),
                },
                Err(e) => self.say(format!("Фраза не сменена: {e}")),
            }
            changed = true;
        }
        let today = Date::today();
        if today != self.today {
            let was_today = self.page.date == self.today;
            self.today = today;
            if let Ok(Some((from, count))) = self.book.carry_over(today) {
                self.say(format!("Перенесено с {}: {count}", long_date(from)));
                self.edited();
            }
            if was_today {
                self.open_page(today);
            }
            changed = true;
        }
        if self.status.as_ref().is_some_and(|(_, at)| at.elapsed() > STATUS_FOR) {
            self.status = None;
            changed = true;
        }
        let page_changed = !self.page_error && self.page.changed_outside(&self.book);
        let lists_changed = self.book.lists_stamp() != self.lists_stamp;
        let notes_changed = self.notes.as_ref().is_some_and(|v| self.book.notes_stamp(&v.id) != v.stamp);
        if page_changed || lists_changed || notes_changed {
            self.check_external();
            changed = true;
        }
        changed
    }

    /// Приложение уходит в фон или возвращается: правки — на сервер сразу.
    pub fn on_pause(&mut self) {
        self.edited_at = None;
        self.worker.now();
    }

    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn on_resume(&mut self) {
        self.worker.now();
    }

    // ------------------------------------------------------------------ экран

    fn show(&self, ui: &AppWindow) {
        ui.set_status(self.status.as_ref().map(|(s, _)| s.as_str()).unwrap_or("").into());
        ui.set_sync_label(self.sync.clone().into());
        ui.set_day_title(day_title(self.page.date, self.today).into());
        ui.set_is_today(self.page.date == self.today);
        ui.set_read_only(self.page_error);

        let tabs: Vec<ListTab> = self
            .shown
            .iter()
            .map(|&c| ListTab {
                title: self.list_title(c).into(),
                count: self.page.board.list(c).iter().filter(|t| t.is_open()).count() as i32,
            })
            .collect();
        ui.set_lists(ModelRc::new(VecModel::from(tabs)));
        ui.set_active_list(self.shown.iter().position(|&c| c == self.active).unwrap_or(0) as i32);

        let summaries = self.book.summaries(&self.page.board);
        let hidden = self.registry.is_hidden(self.active);
        let rows: Vec<TaskRow> = self
            .page
            .board
            .list(self.active)
            .iter()
            .map(|t| {
                let badge = match t.id.as_ref().and_then(|id| summaries.get(id)) {
                    // «✎», как в терминале, в шрифтах Android нет — эмодзи есть всегда.
                    Some(s) if s.subtasks > 0 => format!("📝 {}/{}", s.subtasks_done, s.subtasks),
                    Some(_) => "📝".into(),
                    None => String::new(),
                };
                TaskRow {
                    text: t.text.clone().into(),
                    state: state_code(t.state),
                    migrated: match t.state {
                        State::Migrated(to) => format!("→ {}", short_date(to)).into(),
                        _ => SharedString::new(),
                    },
                    badge: badge.into(),
                    private: t.private || hidden,
                }
            })
            .collect();
        ui.set_tasks(ModelRc::new(VecModel::from(rows)));

        if let Some((category, index)) = self.task_pos() {
            let task = &self.page.board.list(category)[index];
            ui.set_task_state(state_code(task.state));
            ui.set_task_text(task.text.clone().into());
            ui.set_task_history(task.id.as_deref().map(|id| self.task_history(id)).unwrap_or_default().into());
            ui.set_task_list(self.shown.iter().position(|&c| c == category).unwrap_or(0) as i32);
        }

        if let Some(view) = &self.notes {
            let title = self.task_pos().map(|(c, i)| self.page.board.list(c)[i].text.clone()).unwrap_or_default();
            ui.set_notes_title(title.into());
            let rows: Vec<NoteRow> = view
                .shown
                .iter()
                .map(|&i| {
                    let item = view.notes.item(i);
                    let mut text = item.head.to_string();
                    for line in &item.body {
                        text.push('\n');
                        text.push_str(line);
                    }
                    let (kind, done, stamp) = match item.kind {
                        Kind::Subtask { done } => (0, done, ""),
                        Kind::Note { stamp } => (1, false, stamp),
                        Kind::Text => (2, false, ""),
                    };
                    NoteRow { kind, done, stamp: stamp.into(), text: text.into() }
                })
                .collect();
            ui.set_notes(ModelRc::new(VecModel::from(rows)));
        }

        ui.set_encrypted(self.config.webdav().is_some_and(|s| s.is_encrypted()));
    }

    fn show_settings(&self, ui: &AppWindow) {
        let get = |key| SharedString::from(self.config.get(key).unwrap_or(""));
        ui.set_webdav_url(get(config::WEBDAV_URL));
        ui.set_webdav_user(get(config::WEBDAV_USER));
        ui.set_webdav_password(get(config::WEBDAV_PASSWORD));
        ui.set_passphrase(get(config::WEBDAV_PASSPHRASE));
    }

    fn save_settings(&mut self, ui: &AppWindow) {
        let values = [
            (config::WEBDAV_URL, ui.get_webdav_url()),
            (config::WEBDAV_USER, ui.get_webdav_user()),
            (config::WEBDAV_PASSWORD, ui.get_webdav_password()),
            (config::WEBDAV_PASSPHRASE, ui.get_passphrase()),
        ];
        for (key, value) in values {
            let value = value.trim();
            if self.config.get(key).unwrap_or("") == value {
                continue;
            }
            if let Err(e) = self.config.set(key, value) {
                return self.say(format!("Настройки не сохранены: {e:#}"));
            }
        }
        self.reconfigure();
        if self.config.webdav().is_some() {
            self.say("WebDAV: синхронизация…");
        } else {
            self.say("Настройки сохранены");
        }
    }
}

/// Экран назад.
fn go_back(c: &mut Ctx, ui: &AppWindow) {
    let to = match ui.get_screen() {
        Screen::Notes => {
            c.notes = None;
            Screen::Task
        }
        Screen::About => Screen::Settings,
        _ => {
            c.task = None;
            Screen::Page
        }
    };
    ui.set_screen(to);
}

thread_local! {
    /// Состояние приложения — для событий Android (уход в фон), которые приходят не из окна.
    static CTX: RefCell<Option<Rc<RefCell<Ctx>>>> = const { RefCell::new(None) };
}

/// Событие жизненного цикла Android.
pub fn with_ctx(f: impl FnOnce(&mut Ctx)) {
    CTX.with(|c| {
        if let Some(ctx) = c.borrow().as_ref()
            && let Ok(mut ctx) = ctx.try_borrow_mut()
        {
            f(&mut ctx);
        }
    });
}

/// Окно приложения над книжкой в `dir` с настройками в `config`.
pub fn run(dir: PathBuf, config: PathBuf) -> Result<(), slint::PlatformError> {
    clock::init();
    let ui = AppWindow::new()?;
    let ctx = Rc::new(RefCell::new(Ctx::new(dir, config)));
    CTX.with(|c| *c.borrow_mut() = Some(ctx.clone()));
    ctx.borrow().show(&ui);

    // Обработчик: правка в `Ctx`, затем экран заново.
    macro_rules! on {
        ($setter:ident, |$c:ident, $ui:ident $(, $arg:ident)*| $body:expr) => {{
            let ctx = ctx.clone();
            let weak = ui.as_weak();
            ui.$setter(move |$($arg),*| {
                let Some($ui) = weak.upgrade() else { return };
                #[allow(unused_mut)]
                let mut $c = ctx.borrow_mut();
                $body;
                $c.show(&$ui);
            });
        }};
    }

    on!(on_prev_day, |c, ui| {
        let date = c.page.date.add_days(-1);
        c.open_page(date)
    });
    on!(on_next_day, |c, ui| {
        let date = c.page.date.add_days(1);
        c.open_page(date)
    });
    on!(on_go_today, |c, ui| {
        let today = c.today;
        c.open_page(today)
    });
    on!(on_select_list, |c, ui, i| {
        if let Some(&id) = c.shown.get(i as usize) {
            c.active = id;
        }
    });
    on!(on_toggle, |c, ui, i| {
        let active = c.active;
        c.toggle(active, i as usize, false)
    });
    on!(on_open_task, |c, ui, i| {
        if c.open_task(i as usize) {
            ui.set_screen(Screen::Task);
        }
    });
    on!(on_add_task, |c, ui, text| c.add_task(&text));
    on!(on_save_text, |c, ui, text| {
        let text = text.trim().to_string();
        if let Some((category, index)) = c.task_pos()
            && !text.is_empty()
        {
            let task = c.page.board.get_mut(category, index).unwrap();
            task.text = text;
            task.touch();
            c.save();
            c.say("Сохранено");
        }
    });
    on!(on_set_state, |c, ui, state| {
        if let Some((category, index)) = c.task_pos() {
            let state = match state {
                1 => State::Done,
                2 => State::Cancelled,
                _ => State::Open,
            };
            c.page.board.set_state(category, index, state);
            c.save();
            ui.set_screen(Screen::Page);
        }
    });
    on!(on_postpone, |c, ui, text| {
        if c.postpone(&text) {
            ui.set_screen(Screen::Page);
        }
    });
    on!(on_move_to_list, |c, ui, i| {
        if let Some(&to) = c.shown.get(i as usize) {
            c.move_to_list(to);
        }
    });
    on!(on_shift, |c, ui, up| {
        if let Some((category, index)) = c.task_pos() {
            let moved =
                if up { c.page.board.move_up(category, index) } else { c.page.board.move_down(category, index) };
            if moved.is_some() {
                c.save();
            }
        }
    });
    on!(on_delete_task, |c, ui| {
        c.delete_task();
        ui.set_screen(Screen::Page);
    });
    on!(on_open_notes, |c, ui| {
        if c.open_notes() {
            ui.set_screen(Screen::Notes);
        }
    });
    on!(on_add_note, |c, ui, text| c.add_note(&text, false));
    on!(on_add_subtask, |c, ui, text| c.add_note(&text, true));
    on!(on_toggle_note, |c, ui, i| c.toggle_note(i as usize));
    on!(on_delete_note, |c, ui, i| c.delete_note(i as usize));
    on!(on_open_settings, |c, ui| {
        c.show_settings(&ui);
        ui.set_screen(Screen::Settings);
    });
    on!(on_save_settings, |c, ui| c.save_settings(&ui));
    on!(on_sync_now, |c, ui| {
        if c.config.webdav().is_some() {
            c.worker.now();
            c.say("WebDAV: синхронизация…");
        } else {
            c.say("Сначала укажите адрес WebDAV");
        }
    });
    on!(on_change_passphrase, |c, ui, new| {
        let new = new.trim().to_string();
        if new.is_empty() {
            c.say("Новая фраза пустая — не сменена");
        } else {
            c.worker.change_passphrase(new);
            c.say("Шифрование: смена фразы…");
        }
    });
    on!(on_back, |c, ui| go_back(&mut c, &ui));
    {
        let ctx = ctx.clone();
        let weak = ui.as_weak();
        ui.on_back_key(move |popup| {
            let Some(ui) = weak.upgrade() else { return false };
            let mut c = ctx.borrow_mut();
            // Одно нажатие приходит дважды: клавишей и системным жестом «Назад». Второе
            // не должно ни уводить дальше назад, ни закрывать приложение.
            if c.last_back.is_some_and(|t| t.elapsed() < BACK_REPEAT) {
                return true;
            }
            let handled = popup || ui.get_screen() != Screen::Page;
            if handled {
                c.last_back = Some(Instant::now());
                if !popup {
                    go_back(&mut c, &ui);
                }
                c.show(&ui);
            }
            handled
        });
    }

    let timer = Timer::default();
    {
        let ctx = ctx.clone();
        let weak = ui.as_weak();
        timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
            let (Some(ui), Ok(mut c)) = (weak.upgrade(), ctx.try_borrow_mut()) else { return };
            if c.tick() {
                // Задачу, которую открыли, удалили на другом устройстве, — экран закрывается.
                if c.task.is_some() && c.task_pos().is_none() {
                    c.task = None;
                    c.notes = None;
                    ui.set_screen(Screen::Page);
                }
                c.show(&ui);
            }
        });
    }

    let result = ui.run();
    with_ctx(|c| c.on_pause());
    result
}
