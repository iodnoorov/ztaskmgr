//! Состояние программы и обработка событий (в стиле Elm: событие → `App` → отрисовка
//! в `ui.rs`). Здесь — ядро: открытая страница, выделение, главный экран, перенос, сетка
//! списков. Окна — в своих модулях.

mod lists_window;
mod notes;
mod preview;
mod settings;
#[cfg(test)]
mod tests;

pub use lists_window::ListsMode;
pub use notes::{AUTOSAVE, Draft, NoteTarget, NotesMode, NotesView};
pub use preview::Preview;
pub use settings::{Setting, SyncState};

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;

use crate::book::{Book, DELETED_NOTES, Stamp};
use crate::clock::{self, Date};
use crate::config::Config;
use crate::field::{LineInput, TextArea};
use crate::input::{Mouse, MouseKind, Press};
use crate::keymap::{self, Action};
use crate::lists::{Lists, MAX_LISTS};
use crate::model::{Board, ListId, State, Task};
use crate::notes::{Notes, Summary};

#[derive(Debug)]
pub enum Mode {
    Normal,
    /// Ввод текста: новая задача (`index == None`) или правка существующей.
    Edit {
        input: LineInput,
        index: Option<usize>,
    },
    /// Ввод даты: перейти к странице или перенести задачу.
    Date {
        input: LineInput,
        purpose: DatePurpose,
    },
    ConfirmDelete,
    /// Перенос задачи в другой список: выделен вариант с этим номером (`move_targets`).
    MoveTo(usize),
    /// Окно списков: выделена строка `row` (`list_rows`).
    Lists {
        row: usize,
        sub: ListsMode,
    },
    /// Окно настроек; выделен пункт с этим номером (`Setting::ALL`), у текстового пункта
    /// может быть открыто поле.
    Settings {
        row: usize,
        input: Option<LineInput>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatePurpose {
    Go,
    /// Перенос задачи с этим индексом в активном списке.
    Postpone(usize),
}

/// Что на экране под тапом (щелчком): карту попаданий строит отрисовка на каждом кадре.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// Как нажать клавишу: подсказка внизу, стрелки дней в шапке.
    Key(keymap::Key),
    /// Список: его рамка, вкладка, пустое место.
    List(ListId),
    Task(ListId, usize),
    /// `[ ]` у задачи — отметить.
    Check(ListId, usize),
    /// Панель превью: тап — записи задачи, свайп — прокрутка.
    Preview,
    /// Пункт окна настроек — переключить.
    Setting(usize),
    /// Вариант в окне выбора (перенос в список, строка окна списков).
    Pick(usize),
    /// Пункт записей (номер среди видимых) и `[ ]` у подзадачи.
    Entry(usize),
    EntryCheck(usize),
    /// Всплывающее окно: тап мимо его кнопок ничего не делает, до списка под ним не доходит.
    Block,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    /// Сообщение; пропадает при следующем нажатии.
    Info(String),
    /// Ошибка; держится, пока не пройдёт следующее сохранение или не сменится страница.
    Error(String),
}

pub struct App {
    pub book: Book,
    /// Дата открытой страницы.
    pub date: Date,
    pub today: Date,
    pub board: Board,
    /// Страница не прочиталась: показываем пустой лист, но не даём его править и сохранять,
    /// иначе файл с ошибкой перезапишется.
    pub page_error: Option<String>,
    pub active: ListId,
    /// Списки на экране для открытой страницы: из реестра и архивные, в которых на ней
    /// есть задачи (`refresh_shown`), не больше `MAX_LISTS`.
    pub shown: Vec<ListId>,
    /// Выделение и прокрутка — свои у каждого списка.
    pub lists: HashMap<ListId, ListState>,
    pub mode: Mode,
    pub show_help: bool,
    pub show_about: bool,
    pub status: Option<Status>,
    /// Окно записей выделенной задачи.
    pub notes: Option<NotesView>,
    /// Сводка записей задач открытой страницы по id — для значка `✎`.
    pub summaries: HashMap<String, Summary>,
    /// Файл, который надо открыть во внешнем редакторе: это делает главный цикл,
    /// ему принадлежит терминал. Потом — `after_editor`.
    pub editor_request: Option<PathBuf>,
    /// Реестр списков книжки (`LISTS.md`): названия, порядок, скрытые от ИИ.
    pub registry: Lists,
    /// Настройки этого устройства.
    pub config: Config,
    /// Превью выделенной задачи (`preview`) и его прокрутка; прокрутка сбрасывается, когда
    /// выделяют другую задачу.
    preview: Option<Preview>,
    pub preview_scroll: u16,
    preview_target: Option<(Date, ListId, usize)>,
    /// Время изменения файлов страницы и скрытых списков, когда мы их читали или писали.
    page_stamp: Option<Stamp>,
    lists_stamp: Option<Stamp>,
    /// Карта попаданий последнего кадра; позже нарисованное — выше.
    pub hits: Vec<(Rect, Hit)>,
    /// Ширина экрана в последнем кадре.
    pub width: u16,
    /// Было что сохранить — главный цикл попросит синхронизацию (через пару секунд,
    /// чтобы правки подряд ушли одной).
    pub edited: bool,
    /// Пользователь попросил синхронизацию сейчас.
    pub sync_requested: bool,
    /// Поменялись настройки WebDAV.
    pub webdav_changed: bool,
    /// Пользователь попросил сменить фразу шифрования на эту.
    pub passphrase_change: Option<String>,
    pub sync: SyncState,
    pub quit: bool,
}

fn ctrl(press: &Press, c: char) -> bool {
    press.hotkey.code == KeyCode::Char(c) && press.hotkey.modifiers.contains(KeyModifiers::CONTROL)
}

/// Правка однострочного поля. Возвращает `Enter` или `Esc`, если поле пора закрыть.
fn edit_line(input: &mut LineInput, press: &Press) -> Option<KeyCode> {
    let typed = press.typed;
    match press.hotkey.code {
        code @ (KeyCode::Esc | KeyCode::Enter) => return Some(code),
        _ if ctrl(press, 'w') => input.delete_word(),
        _ if ctrl(press, 'a') => input.home(),
        _ if ctrl(press, 'e') => input.end(),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.left(),
        KeyCode::Right => input.right(),
        KeyCode::Home => input.home(),
        KeyCode::End => input.end(),
        _ => {
            if let KeyCode::Char(c) = typed.code
                && !typed.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                input.insert(c);
            }
        }
    }
    None
}

impl App {
    pub fn new(book: Book, config: Config, today: Date) -> Self {
        let mut app = Self {
            book,
            date: today,
            today,
            board: Board::default(),
            page_error: None,
            active: ListId::WORK,
            shown: Vec::new(),
            lists: HashMap::new(),
            mode: Mode::Normal,
            show_help: false,
            show_about: false,
            status: None,
            notes: None,
            summaries: HashMap::new(),
            editor_request: None,
            registry: Lists::default(),
            config,
            preview: None,
            preview_scroll: 0,
            preview_target: None,
            page_stamp: None,
            lists_stamp: None,
            hits: Vec::new(),
            width: 0,
            edited: false,
            sync_requested: false,
            webdav_changed: false,
            passphrase_change: None,
            sync: SyncState::Off,
            quit: false,
        };
        app.load_registry();
        app.open_page(today);
        app
    }

    pub fn selected(&self) -> Option<usize> {
        self.lists.get(&self.active)?.selected()
    }

    /// Выделение и прокрутка списка.
    pub fn list_state(&mut self, category: ListId) -> &mut ListState {
        self.lists.entry(category).or_default()
    }

    /// Название списка: из реестра, для незнакомого — из заголовка страницы.
    pub fn list_title(&self, category: ListId) -> &str {
        self.registry.title(category).or(self.board.title(category)).unwrap_or(category.as_str())
    }

    /// Какие списки на экране; активный — один из них.
    fn refresh_shown(&mut self) {
        let mut shown = self.registry.order(self.board.ids());
        shown.truncate(MAX_LISTS);
        self.shown = shown;
        if !self.shown.contains(&self.active) {
            self.active = self.shown[0];
        }
        for category in self.shown.clone() {
            self.clamp_selection(category);
        }
    }

    pub fn selected_task(&self) -> Option<&Task> {
        self.board.list(self.active).get(self.selected()?)
    }

    fn select(&mut self, category: ListId, index: Option<usize>) {
        self.list_state(category).select(index);
        self.clamp_selection(category);
    }

    /// Держит выделение в границах списка; у непустого списка что-то выделено всегда.
    fn clamp_selection(&mut self, category: ListId) {
        let len = self.board.list(category).len();
        let state = self.list_state(category);
        state.select(match (len, state.selected()) {
            (0, _) => None,
            (_, None) => Some(0),
            (_, Some(i)) => Some(i.min(len - 1)),
        });
    }

    /// Открывает страницу на дату; активный список остаётся тем же.
    pub fn open_page(&mut self, date: Date) {
        // Окно записей закроется — начатую заметку сохраняем, пока страница прежняя.
        self.save_draft(false);
        self.date = date;
        match self.book.load(date) {
            Ok(board) => {
                self.board = board;
                self.page_error = None;
                if matches!(self.status, Some(Status::Error(_))) {
                    self.status = None;
                }
            }
            Err(e) => {
                let e = format!("{e:#}");
                self.board = Board::default();
                self.status = Some(Status::Error(format!("Страница не прочитана: {e}")));
                self.page_error = Some(e);
            }
        }
        self.lists.clear();
        self.refresh_shown();
        self.page_stamp = self.book.page_stamp(date);
        // Окно записей привязано к задаче прежней страницы.
        self.notes = None;
        self.summaries = self.book.summaries(&self.board);
    }

    /// Наступил новый день (программа открыта через полночь): автоперенос, и если была
    /// открыта страница «сегодня» — переход на новую.
    pub fn on_new_day(&mut self, today: Date) {
        let was_today = self.date == self.today;
        self.today = today;
        match self.book.carry_over(today) {
            Ok(carried) => {
                if let Some((from, count)) = carried {
                    self.status = Some(Status::Info(carried_message(from, count)));
                    self.edited = true;
                }
                // Страницы, которых коснулся перенос, перечитываются.
                if was_today || self.date == today || carried.is_some_and(|(f, _)| f == self.date) {
                    self.open_page(if was_today { today } else { self.date });
                }
            }
            Err(e) => self.status = Some(Status::Error(format!("Автоперенос: {e:#}"))),
        }
    }

    fn save(&mut self) {
        // Файл поменяли снаружи с тех пор, как мы его читали (синхронизация в фоне, агент),
        // и мы этого ещё не видели — не затираем, а сливаем с ним.
        if self.book.page_stamp(self.date) != self.page_stamp
            && let Ok(disk) = self.book.load(self.date)
        {
            self.board = crate::merge::merge_boards(&self.board, &disk);
            self.refresh_shown();
        }
        let book = &self.book;
        self.board.assign_ids(|| book.new_id());
        self.edited = true;
        match self.book.save(self.date, &self.board) {
            Ok(()) => {
                if matches!(self.status, Some(Status::Error(_))) {
                    self.status = None;
                }
            }
            Err(e) => self.status = Some(Status::Error(format!("Ошибка сохранения: {e:#}"))),
        }
        self.page_stamp = self.book.page_stamp(self.date);
    }

    fn load_registry(&mut self) {
        match self.book.lists() {
            Ok(lists) => self.registry = lists,
            Err(e) => self.status = Some(Status::Error(format!("Списки: {e:#}"))),
        }
        if let Some(e) = self.book.lists_error() {
            self.status = Some(Status::Error(format!("{e} — пока он испорчен, в ходу исходные списки")));
        }
        self.lists_stamp = self.book.lists_stamp();
        if !self.shown.is_empty() {
            self.refresh_shown();
        }
    }

    /// Меняет реестр списков и сохраняет его; ошибка — в строку состояния.
    fn edit_registry(&mut self, edit: impl FnOnce(&mut Lists) -> anyhow::Result<()>) -> bool {
        // Реестр поменяли снаружи — правка ложится на свежую версию, а не затирает её.
        if self.book.lists_stamp() != self.lists_stamp {
            self.load_registry();
        }
        let mut lists = self.registry.clone();
        let result = edit(&mut lists).and_then(|()| self.book.save_lists(&lists));
        self.lists_stamp = self.book.lists_stamp();
        match result {
            Ok(()) => {
                self.registry = lists;
                self.refresh_shown();
                self.edited = true;
                true
            }
            Err(e) => {
                self.status = Some(Status::Error(format!("{e:#}")));
                false
            }
        }
    }

    /// Скрыт ли список от ИИ.
    pub fn is_hidden(&self, category: ListId) -> bool {
        self.registry.is_hidden(category)
    }

    fn toggle_hidden_of(&mut self, category: ListId) {
        let hidden = !self.is_hidden(category);
        if self.edit_registry(|lists| lists.set_hidden(category, hidden)) {
            let what = if hidden { "скрыт от ИИ" } else { "открыт для ИИ" };
            self.status = Some(Status::Info(format!("Список «{}» {what}", self.list_title(category))));
        }
    }

    fn toggle_hidden(&mut self) {
        self.toggle_hidden_of(self.active);
    }

    fn toggle_private(&mut self) {
        let Some(index) = self.selected() else {
            return;
        };
        let Some(task) = self.board.get_mut(self.active, index) else {
            return;
        };
        task.private ^= true;
        task.touch();
        let what = if task.private { "скрыта от ИИ" } else { "открыта для ИИ" };
        self.status = Some(Status::Info(format!("Задача {what}")));
        self.save();
    }

    /// Задача, как её найти снова после перечитывания страницы: по id, иначе по тексту —
    /// сначала среди задач без id, потом среди всех (снаружи ей могли дать id).
    fn find_task(&self, category: ListId, task: &Task) -> Option<usize> {
        let list = self.board.list(category);
        match &task.id {
            Some(id) => list.iter().position(|t| t.id.as_ref() == Some(id)),
            None => list
                .iter()
                .position(|t| t.id.is_none() && t.text == task.text)
                .or_else(|| list.iter().position(|t| t.text == task.text)),
        }
    }

    /// Файлы страницы, открытых записей или скрытых списков поменялись снаружи (агент,
    /// синхронизация, правка руками) — перечитать, сохранив выделение и начатую правку.
    /// Возвращает, было ли что перечитывать.
    pub fn check_external(&mut self) -> bool {
        let mut changed = false;
        let lists_changed = self.book.lists_stamp() != self.lists_stamp;
        if lists_changed {
            self.load_registry();
            changed = true;
        }
        // Страница зависит от реестра (названия, архивные списки) — перечитываем и её.
        if lists_changed || self.book.page_stamp(self.date) != self.page_stamp {
            self.reload_page();
            changed = true;
        }
        if let Some(view) = &self.notes
            && self.book.notes_stamp(&view.id) != view.stamp
        {
            self.reload_notes();
            changed = true;
        }
        if changed && self.status.is_none() {
            self.status = Some(Status::Info("Обновлено снаружи".into()));
        }
        changed
    }

    fn reload_page(&mut self) {
        // Что было выделено и что правится — чтобы найти те же задачи на перечитанной странице.
        let selected: HashMap<ListId, Task> = self
            .lists
            .iter()
            .filter_map(|(&c, state)| Some((c, self.board.list(c).get(state.selected()?)?.clone())))
            .collect();
        let editing = match &self.mode {
            Mode::Edit { index: Some(i), .. } | Mode::Date { purpose: DatePurpose::Postpone(i), .. } => {
                self.board.list(self.active).get(*i).cloned()
            }
            _ => None,
        };
        let notes_task = self.notes_task().cloned();

        match self.book.load(self.date) {
            Ok(board) => {
                self.board = board;
                self.page_error = None;
            }
            Err(e) => {
                let e = format!("{e:#}");
                self.board = Board::default();
                self.status = Some(Status::Error(format!("Страница не прочитана: {e}")));
                self.page_error = Some(e);
            }
        }
        self.page_stamp = self.book.page_stamp(self.date);

        for (&category, task) in &selected {
            let index = self.find_task(category, task);
            self.lists.entry(category).or_default().select(index);
        }
        self.refresh_shown();
        let editing = editing.map(|t| self.find_task(self.active, &t));
        let selected_gone = selected.get(&self.active).is_some_and(|t| self.find_task(self.active, t).is_none());
        match &mut self.mode {
            Mode::Edit { index: Some(i), .. } | Mode::Date { purpose: DatePurpose::Postpone(i), .. } => {
                match editing.flatten() {
                    Some(j) => *i = j,
                    None => {
                        self.mode = Mode::Normal;
                        self.status = Some(Status::Info("Задача изменена снаружи — правка закрыта".into()));
                    }
                }
            }
            Mode::ConfirmDelete | Mode::MoveTo(_) if selected_gone => self.mode = Mode::Normal,
            _ => {}
        }
        if let (Some(task), Some(view)) = (notes_task, &self.notes) {
            match self.find_task(view.category, &task) {
                Some(j) => {
                    let new_id = self.board.list(view.category)[j].id.clone();
                    let view = self.notes.as_mut().unwrap();
                    view.index = j;
                    // Задаче без записей дали id снаружи (агент добавил запись): у окна был
                    // лишь заготовленный id без файла — берём настоящий и читаем записи.
                    if let Some(id) = new_id
                        && id != view.id
                    {
                        view.id = id;
                        view.stamp = None;
                        self.reload_notes();
                    }
                }
                None => {
                    // Записи живут по id — начатое сохраняем, окно закрываем.
                    self.save_draft(false);
                    self.notes = None;
                    self.status = Some(Status::Info("Задача изменена снаружи — записи закрыты".into()));
                }
            }
        }
        self.summaries = self.book.summaries(&self.board);
    }

    /// Можно ли править открытую страницу; если нет — говорит почему.
    fn editable(&mut self) -> bool {
        if self.page_error.is_some() {
            self.status = Some(Status::Error("Страница не прочитана — правка отключена, исправьте файл".into()));
        }
        self.page_error.is_none()
    }

    /// Нажатие: горячие клавиши — по физической клавише, поле ввода получает набранное.
    pub fn on_key(&mut self, press: impl Into<Press>) {
        let press = press.into();
        if ctrl(&press, 'c') {
            // Начатая заметка не пропадает и при выходе.
            self.save_draft(false);
            self.quit = true;
            return;
        }
        if matches!(self.status, Some(Status::Info(_))) {
            self.status = None;
        }
        // Действие — над тем, что сейчас на диске, а не над устаревшей копией.
        self.check_external();
        if matches!(self.status, Some(Status::Info(_))) {
            self.status = None;
        }
        if self.show_help || self.show_about {
            self.show_help = false;
            self.show_about = false;
            return;
        }
        if self.notes.is_some() {
            self.on_key_notes(press);
            return;
        }
        match self.mode {
            Mode::Normal => {
                let key = (press.hotkey.code, press.hotkey.modifiers);
                if let Some(action) = keymap::action(key) {
                    self.act(action);
                }
            }
            Mode::Edit { .. } => self.on_key_edit(press),
            Mode::Date { .. } => self.on_key_date(press),
            Mode::ConfirmDelete => self.on_key_confirm(press),
            Mode::MoveTo(_) => self.on_key_move_to(press),
            Mode::Lists { .. } => self.on_key_lists(press),
            Mode::Settings { .. } => self.on_key_settings(press),
        }
    }

    /// Открыто ли поле ввода. Тогда отслеживание мыши выключается: в Termux тап по экрану
    /// снова вызывает клавиатуру, а не уходит программе.
    pub fn wants_text(&self) -> bool {
        let notes_field = self.notes.as_ref().is_some_and(|v| {
            matches!(v.mode, NotesMode::Edit { .. } | NotesMode::Text(Draft { confirm_close: false, .. }))
        });
        let lists_field = matches!(
            self.mode,
            Mode::Lists { sub: ListsMode::Add(_) | ListsMode::Rename(_), .. } | Mode::Settings { input: Some(_), .. }
        );
        notes_field
            || (self.notes.is_none() && (lists_field || matches!(self.mode, Mode::Edit { .. } | Mode::Date { .. })))
    }

    /// Тап (щелчок) и свайп (прокрутка): что под пальцем — по карте попаданий кадра.
    pub fn on_mouse(&mut self, mouse: Mouse) {
        let point = Position::new(mouse.column, mouse.row);
        let hit = self.hits.iter().rev().find(|(rect, _)| rect.contains(point)).map(|&(_, hit)| hit);
        if self.show_help || self.show_about {
            // Окна, которые закрываются любой клавишей, закрываются и тапом.
            if mouse.kind == MouseKind::Click {
                self.on_key(KeyCode::Esc);
            }
            return;
        }
        let scroll = match mouse.kind {
            MouseKind::Click => {
                if let Some(hit) = hit {
                    self.on_hit(hit);
                }
                return;
            }
            MouseKind::ScrollUp => Action::Up,
            MouseKind::ScrollDown => Action::Down,
        };
        self.check_external();
        match &self.notes {
            Some(NotesView { mode: NotesMode::List, .. }) => self.act_notes(scroll),
            Some(_) => {}
            None if matches!(self.mode, Mode::Normal) && hit == Some(Hit::Preview) => {
                self.act(if scroll == Action::Up { Action::PreviewUp } else { Action::PreviewDown });
            }
            None if matches!(self.mode, Mode::Normal) => {
                if let Some(Hit::List(c) | Hit::Task(c, _) | Hit::Check(c, _)) = hit {
                    self.active = c;
                }
                self.act(scroll);
            }
            None => {}
        }
    }

    fn on_hit(&mut self, hit: Hit) {
        if let Hit::Key((code, modifiers)) = hit {
            self.on_key(KeyEvent::new(code, modifiers));
            return;
        }
        self.check_external();
        if matches!(self.status, Some(Status::Info(_))) {
            self.status = None;
        }
        if let Some(view) = &mut self.notes {
            if !matches!(view.mode, NotesMode::List) {
                return;
            }
            match hit {
                // Тап по выделенному пункту — правка, по другому — выделить.
                Hit::Entry(n) if view.list.selected() == Some(n) => self.act_notes(Action::Edit),
                Hit::Entry(n) => view.list.select(Some(n)),
                Hit::EntryCheck(n) => {
                    view.list.select(Some(n));
                    self.act_notes(Action::Toggle);
                }
                _ => {}
            }
            return;
        }
        if let (Mode::Settings { row, input: None }, Hit::Setting(i)) = (&mut self.mode, hit) {
            *row = i;
            self.on_key_settings(Press::from(KeyCode::Enter));
            return;
        }
        // Тап по варианту в окне выбора: перенести туда задачу, выделить строку списков,
        // перенести туда задачи удаляемого списка.
        if let Hit::Pick(i) = hit {
            match &mut self.mode {
                Mode::MoveTo(_) => {
                    self.mode = Mode::Normal;
                    if let Some(&to) = self.move_targets().get(i) {
                        self.move_task_to(to);
                    }
                }
                Mode::Lists { row, sub: ListsMode::Browse } => *row = i,
                Mode::Lists { sub: ListsMode::Delete { target, count }, .. } if *count > 0 => {
                    *target = i;
                    self.on_key_lists(Press::from(KeyEvent::from(KeyCode::Enter)));
                }
                _ => {}
            }
            return;
        }
        if !matches!(self.mode, Mode::Normal) {
            return;
        }
        match hit {
            Hit::List(c) => self.active = c,
            Hit::Preview => self.act(Action::Notes),
            // Тап по выделенной задаче — её записи, по другой — выделить.
            Hit::Task(c, i) if self.active == c && self.selected() == Some(i) => self.act(Action::Notes),
            Hit::Task(c, i) => {
                self.active = c;
                self.select(c, Some(i));
            }
            Hit::Check(c, i) => {
                self.active = c;
                self.select(c, Some(i));
                self.act(Action::Toggle);
            }
            _ => {}
        }
    }

    /// Вставленный текст идёт в открытое поле ввода как есть, иначе игнорируется:
    /// на главном экране он сработал бы как горячие клавиши.
    pub fn on_paste(&mut self, text: &str) {
        let input = match (&mut self.notes, &mut self.mode) {
            (Some(view), _) => match &mut view.mode {
                NotesMode::Edit { input, .. } => Some(input),
                // В многострочное — с переводами строк.
                NotesMode::Text(Draft { area, .. }) => {
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    text.chars().filter(|&c| c == '\n' || !c.is_control()).for_each(|c| area.insert(c));
                    return;
                }
                _ => None,
            },
            (None, Mode::Edit { input, .. } | Mode::Date { input, .. }) => Some(input),
            (None, _) => None,
        };
        if let Some(input) = input {
            text.chars().filter(|c| !c.is_control()).for_each(|c| input.insert(c));
        }
    }

    fn act(&mut self, action: Action) {
        let edits = matches!(
            action,
            Action::Add
                | Action::Edit
                | Action::Toggle
                | Action::Cancel
                | Action::MoveUp
                | Action::MoveDown
                | Action::MoveTo
                | Action::Postpone
                | Action::Delete
                | Action::Notes
                | Action::TogglePrivate
        );
        if edits && !self.editable() {
            return;
        }
        match action {
            Action::Quit => self.quit = true,
            Action::Help => self.show_help = true,
            Action::About => self.show_about = true,
            Action::List(i) => {
                if let Some(&c) = self.shown.get(i) {
                    self.active = c;
                }
            }
            Action::NextList => self.cycle_list(true),
            Action::PrevList => self.cycle_list(false),
            Action::ListLeft | Action::ListRight | Action::ListUp | Action::ListDown => self.grid_move(action),
            Action::Lists => {
                let row = self.list_rows().iter().position(|&c| c == self.active).unwrap_or(0);
                self.mode = Mode::Lists { row, sub: ListsMode::Browse };
            }
            // select_previous/next у пустого списка выставляют индекс за границей — поправляем.
            Action::Up => {
                self.list_state(self.active).select_previous();
                self.clamp_selection(self.active);
            }
            Action::Down => {
                self.list_state(self.active).select_next();
                self.clamp_selection(self.active);
            }
            Action::Top => self.select(self.active, Some(0)),
            Action::Bottom => self.select(self.active, Some(usize::MAX)),
            Action::PrevDay => self.open_page(self.date.add_days(-1)),
            Action::NextDay => self.open_page(self.date.add_days(1)),
            Action::Today => self.open_page(self.today),
            Action::GoToDate => self.mode = Mode::Date { input: LineInput::default(), purpose: DatePurpose::Go },
            Action::Add => self.mode = Mode::Edit { input: LineInput::default(), index: None },
            Action::Edit => {
                if let (Some(index), Some(task)) = (self.selected(), self.selected_task()) {
                    self.mode = Mode::Edit { input: LineInput::new(task.text.clone()), index: Some(index) };
                }
            }
            Action::Toggle => self.toggle_done(),
            Action::Cancel => self.toggle_cancelled(),
            Action::MoveUp => self.move_task(Board::move_up),
            Action::MoveDown => self.move_task(Board::move_down),
            Action::Postpone => self.start_postpone(),
            Action::MoveTo if self.selected_task().is_some() => {
                if self.move_targets().is_empty() {
                    self.status = Some(Status::Info("Других списков нет".into()));
                } else {
                    self.mode = Mode::MoveTo(0);
                }
            }
            Action::Delete if self.selected_task().is_some() => self.mode = Mode::ConfirmDelete,
            Action::Notes => self.open_notes(),
            Action::TogglePrivate => self.toggle_private(),
            Action::ToggleHidden => self.toggle_hidden(),
            Action::Settings => self.mode = Mode::Settings { row: 0, input: None },
            Action::Preview => self.toggle_preview(),
            Action::PreviewDown => self.preview_scroll = self.preview_scroll.saturating_add(1),
            Action::PreviewUp => self.preview_scroll = self.preview_scroll.saturating_sub(1),
            Action::MoveTo | Action::Delete => {}
            // Только в окне записей.
            Action::AddSubtask | Action::OpenEditor | Action::Close => {}
        }
    }

    fn on_key_edit(&mut self, press: Press) {
        let Mode::Edit { input, index } = &mut self.mode else {
            return;
        };
        match edit_line(input, &press) {
            Some(KeyCode::Enter) => {
                let text = input.text().trim().to_string();
                let index = *index;
                self.mode = Mode::Normal;
                self.commit_edit(index, text);
            }
            Some(_) => self.mode = Mode::Normal,
            None => {}
        }
    }

    /// Дата из поля ввода; относительные (`+1`) — от открытой страницы.
    pub fn input_date(&self, input: &LineInput) -> Option<Date> {
        Date::parse_input(input.text(), self.date)
    }

    fn on_key_date(&mut self, press: Press) {
        let Mode::Date { input, purpose } = &mut self.mode else {
            return;
        };
        let purpose = *purpose;
        match edit_line(input, &press) {
            Some(KeyCode::Enter) => {
                if input.text().trim().is_empty() {
                    self.mode = Mode::Normal;
                    return;
                }
                // Не дата — поле остаётся открытым, окно показывает, что не так.
                let Some(date) = Date::parse_input(input.text(), self.date) else {
                    return;
                };
                self.mode = Mode::Normal;
                match purpose {
                    DatePurpose::Go => self.open_page(date),
                    DatePurpose::Postpone(index) => self.postpone(index, date),
                }
            }
            Some(_) => self.mode = Mode::Normal,
            None => {}
        }
    }

    fn on_key_confirm(&mut self, press: Press) {
        self.mode = Mode::Normal;
        if matches!(press.hotkey.code, KeyCode::Char('y') | KeyCode::Enter)
            && let Some(index) = self.selected()
        {
            let task = self.board.delete(self.active, index);
            self.clamp_selection(self.active);
            self.save();
            if let Some(task) = task {
                self.archive_notes(&task);
            }
        }
    }

    /// Следующий / предыдущий список на экране, по кругу.
    fn cycle_list(&mut self, forward: bool) {
        let n = self.shown.len();
        let i = self.shown.iter().position(|&c| c == self.active).unwrap_or(0);
        self.active = self.shown[if forward { (i + 1) % n } else { (i + n - 1) % n }];
    }

    /// Соседний список в сетке; на узком экране, где списки — вкладки, — соседняя вкладка.
    fn grid_move(&mut self, action: Action) {
        let n = self.shown.len();
        let Some(i) = self.shown.iter().position(|&c| c == self.active) else {
            return;
        };
        if self.width > 0 && self.width < crate::ui::NARROW {
            return self.cycle_list(matches!(action, Action::ListRight | Action::ListDown));
        }
        let (cols, rows) = crate::ui::grid(n);
        let j = match action {
            Action::ListLeft => (i % cols > 0).then(|| i - 1),
            Action::ListRight => (i % cols + 1 < cols && i + 1 < n).then_some(i + 1),
            Action::ListUp => i.checked_sub(cols),
            // В неполный последний ряд — на его последний список.
            Action::ListDown => (i + cols < n).then_some(i + cols).or((i / cols + 1 < rows).then_some(n - 1)),
            _ => None,
        };
        if let Some(j) = j {
            self.active = self.shown[j];
        }
    }

    /// Куда можно перенести задачу: списки на экране из реестра, кроме текущего.
    pub fn move_targets(&self) -> Vec<ListId> {
        self.registry.active().into_iter().filter(|&c| c != self.active).collect()
    }

    fn on_key_move_to(&mut self, press: Press) {
        let targets = self.move_targets();
        let Mode::MoveTo(selected) = &mut self.mode else {
            return;
        };
        let to = match press.hotkey.code {
            KeyCode::Char(c @ '1'..='9') => targets.get(c as usize - '1' as usize).copied(),
            KeyCode::Enter => targets.get(*selected).copied(),
            KeyCode::Char('j') | KeyCode::Down => {
                *selected = (*selected + 1).min(targets.len().saturating_sub(1));
                return;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                *selected = selected.saturating_sub(1);
                return;
            }
            _ => None,
        };
        self.mode = Mode::Normal;
        if let Some(to) = to {
            self.move_task_to(to);
        }
    }

    fn move_task_to(&mut self, to: ListId) {
        let Some(index) = self.selected() else {
            return;
        };
        let from = self.active;
        if let Some(new_index) = self.board.move_to(from, index, to) {
            // Из скрытого списка в открытый задача уходит скрытой: открыть её может только
            // пользователь. В скрытый — скрыта и так.
            if self.is_hidden(from) && !self.is_hidden(to) {
                self.board.get_mut(to, new_index).unwrap().private = true;
                self.status = Some(Status::Info("Задача осталась скрытой от ИИ (p — открыть)".into()));
            }
            self.clamp_selection(from);
            self.select(to, Some(new_index));
            self.save();
            self.refresh_shown();
        }
    }

    /// Окно даты для переноса; по умолчанию — день после открытой страницы, но не раньше
    /// завтрашнего.
    fn start_postpone(&mut self) {
        let Some(index) = self.selected() else {
            return;
        };
        if !self.board.list(self.active)[index].is_open() {
            self.status = Some(Status::Info("Переносятся только невыполненные задачи".into()));
            return;
        }
        let default = self.date.max(self.today).add_days(1);
        self.mode = Mode::Date { input: LineInput::new(default.to_string()), purpose: DatePurpose::Postpone(index) };
    }

    fn postpone(&mut self, index: usize, to: Date) {
        if to == self.date {
            self.status = Some(Status::Info("Это та же страница".into()));
            return;
        }
        match self.book.postpone(&mut self.board, self.date, self.active, index, to) {
            Ok(true) => {
                self.page_stamp = self.book.page_stamp(self.date);
                self.edited = true;
                let when = to.format(c"%-d %B %Y");
                self.status = Some(Status::Info(format!("Перенесено на {when}")));
            }
            Ok(false) => {}
            Err(e) => self.status = Some(Status::Error(format!("Перенос: {e:#}"))),
        }
    }

    /// Пустой текст отменяет ввод: пустых задач не создаём, существующую не затираем.
    fn commit_edit(&mut self, index: Option<usize>, text: String) {
        if text.is_empty() {
            return;
        }
        match index {
            None => {
                let mut task = Task::new(text);
                task.touch();
                let new_index = self.board.push(self.active, task);
                self.select(self.active, Some(new_index));
            }
            Some(index) => match self.board.get_mut(self.active, index) {
                Some(task) => {
                    task.text = text;
                    task.touch();
                }
                None => return,
            },
        }
        self.save();
    }

    fn toggle_done(&mut self) {
        let Some(index) = self.selected() else {
            return;
        };
        let Some(task) = self.board.list(self.active).get(index) else {
            return;
        };
        // Отменённую всё-таки сделали — выполнена.
        let state = match task.state {
            State::Open | State::Cancelled => State::Done,
            State::Done => State::Open,
            State::Migrated(to) => return self.migrated_notice(to),
        };
        self.set_state(index, state);
    }

    fn toggle_cancelled(&mut self) {
        let Some(index) = self.selected() else {
            return;
        };
        let Some(task) = self.board.list(self.active).get(index) else {
            return;
        };
        let state = match task.state {
            State::Open | State::Done => State::Cancelled,
            State::Cancelled => State::Open,
            State::Migrated(to) => return self.migrated_notice(to),
        };
        self.set_state(index, state);
    }

    fn migrated_notice(&mut self, to: Date) {
        let when = to.format(c"%-d %B");
        self.status = Some(Status::Info(format!("Задача перенесена на {when} — отметьте её там")));
    }

    /// Закрытая задача уезжает вниз, а выделение остаётся на месте — на следующей задаче:
    /// так удобно отмечать подряд. Снятая отметка — выделение едет за задачей.
    fn set_state(&mut self, index: usize, state: State) {
        let moved = self.board.set_state(self.active, index, state);
        if state == State::Open {
            self.select(self.active, moved);
        }
        self.save();
    }

    /// Сдвиг внутри списка (`Board::move_up` / `move_down`).
    fn move_task(&mut self, shift: fn(&mut Board, ListId, usize) -> Option<usize>) {
        let Some(index) = self.selected() else {
            return;
        };
        if let Some(new_index) = shift(&mut self.board, self.active, index) {
            self.select(self.active, Some(new_index));
            self.save();
        }
    }
}

pub fn carried_message(from: Date, count: usize) -> String {
    format!("Перенесено невыполненных задач со страницы {}: {count}", from.format(c"%-d %B"))
}
