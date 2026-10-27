use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;

use crate::book::{Book, Stamp, DELETED_NOTES};
use crate::clock::{self, Date};
use crate::config::Config;
use crate::lists::{Lists, MAX_LISTS};
use crate::field::{LineInput, TextArea};
use crate::input::{Mouse, MouseKind, Press};
use crate::keymap::{self, Action};
use crate::model::{Board, ListId, State, Task};
use crate::notes::{Notes, Summary};

#[derive(Debug)]
pub enum Mode {
    Normal,
    /// Ввод текста: новая задача (`index == None`) или правка существующей.
    Edit { input: LineInput, index: Option<usize> },
    /// Ввод даты: перейти к странице или перенести задачу.
    Date { input: LineInput, purpose: DatePurpose },
    ConfirmDelete,
    /// Перенос задачи в другой список: выделен вариант с этим номером (`move_targets`).
    MoveTo(usize),
    /// Окно списков: выделена строка `row` (`list_rows`).
    Lists { row: usize, sub: ListsMode },
    /// Окно настроек; выделен пункт с этим номером (`Setting::ALL`).
    Settings(usize),
}

/// Что делается в окне списков.
#[derive(Debug)]
pub enum ListsMode {
    Browse,
    /// Название нового списка.
    Add(LineInput),
    Rename(LineInput),
    /// Удаление выделенного списка. С задачами на сегодня и дальше (`count`) — выбор, куда
    /// их перенести (`target` — номер в `delete_targets`); без них — просто подтверждение.
    Delete { count: usize, target: usize },
}

/// Пункт окна настроек.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    Preview,
}

impl Setting {
    pub const ALL: [Setting; 1] = [Setting::Preview];

    pub fn title(self) -> &'static str {
        match self {
            Setting::Preview => "Превью задачи справа",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Setting::Preview => "на экране от 120 колонок · v — переключить",
        }
    }

    pub fn is_on(self, config: &Config) -> bool {
        match self {
            Setting::Preview => config.preview(),
        }
    }
}

/// Что показывает превью: история задачи по страницам и её записи. Собирается заново, когда
/// меняется задача или файлы её страницы и записей (`key`).
pub struct Preview {
    key: (Option<String>, Option<Stamp>, Option<Stamp>),
    pub history: Vec<(Date, ListId, State)>,
    pub notes: Notes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatePurpose {
    Go,
    /// Перенос задачи с этим индексом в активном списке.
    Postpone(usize),
}

/// Окно доп. записей задачи, поверх страницы.
pub struct NotesView {
    pub category: ListId,
    /// Индекс задачи в списке; пока окно открыто, страница не меняется.
    pub index: usize,
    /// id записей; у задачи без записей — новый, задаче он достаётся с первой записью.
    pub id: String,
    pub notes: Notes,
    /// Выделение — среди пунктов, `Notes::visible`.
    pub list: ListState,
    pub mode: NotesMode,
    /// Время изменения файла записей, когда мы его читали или писали, — для правок снаружи.
    stamp: Option<Stamp>,
}

pub enum NotesMode {
    List,
    /// Однострочное поле — подзадача. `original` — текст до правки: по нему пункт находится
    /// снова, если записи поменялись снаружи.
    Edit {
        input: LineInput,
        target: NoteTarget,
        original: String,
    },
    /// Многострочное поле — заметка или прочий текст: Enter — новая строка, Ctrl+S — сохранить.
    Text(Draft),
    ConfirmDelete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteTarget {
    Note,
    Subtask,
    /// Правка пункта с этим номером.
    Entry(usize),
}

/// Как часто искать конфликтные копии Syncthing, пока программа открыта.
const MERGE_EVERY: Duration = Duration::from_secs(2);

/// Как часто сохранять заметку, пока её пишут.
pub const AUTOSAVE: Duration = Duration::from_secs(10);

/// Многострочная заметка в работе. Сохраняется сама раз в `AUTOSAVE`; Esc закрывает поле,
/// не трогая уже сохранённое, а при несохранённых изменениях сначала спрашивает.
pub struct Draft {
    pub area: TextArea,
    /// `Note` — новая заметка до первого сохранения, потом (и при правке) — `Entry`.
    pub target: NoteTarget,
    /// Метка новой заметки — когда её начали писать.
    stamp: String,
    /// Текст на момент последнего сохранения.
    saved: String,
    saved_at: Instant,
    /// Когда сохранилось в последний раз (`20:46:10`) — для показа.
    pub saved_label: Option<String>,
    /// Esc при несохранённых изменениях: окно вопроса.
    pub confirm_close: bool,
}

/// Пустые строки по краям заметки не нужны, внутри — остаются.
fn normalize(text: &str) -> String {
    text.trim_end().trim_start_matches('\n').to_string()
}

impl Draft {
    fn new(text: String, target: NoteTarget) -> Self {
        Self {
            saved: normalize(&text),
            area: TextArea::new(text),
            target,
            stamp: clock::stamp(),
            saved_at: Instant::now(),
            saved_label: None,
            confirm_close: false,
        }
    }

    /// Изменился ли текст с последнего сохранения.
    pub fn is_changed(&self) -> bool {
        normalize(self.area.text()) != self.saved
    }

    /// Стёрт ли весь текст.
    pub fn is_empty(&self) -> bool {
        self.area.text().trim().is_empty()
    }
}

/// Чем закончилась правка поля.
enum Done {
    Save,
    Cancel,
}

/// Правка многострочного поля. Возвращает, если поле пора закрыть.
fn edit_area(area: &mut TextArea, press: &Press) -> Option<Done> {
    let typed = press.typed;
    match press.hotkey.code {
        KeyCode::Esc => return Some(Done::Cancel),
        _ if ctrl(press, 's') => return Some(Done::Save),
        KeyCode::Enter => area.newline(),
        _ if ctrl(press, 'w') => area.delete_word(),
        _ if ctrl(press, 'a') => area.home(),
        _ if ctrl(press, 'e') => area.end(),
        KeyCode::Backspace => area.backspace(),
        KeyCode::Delete => area.delete(),
        KeyCode::Left => area.left(),
        KeyCode::Right => area.right(),
        KeyCode::Up => area.up(),
        KeyCode::Down => area.down(),
        KeyCode::Home => area.home(),
        KeyCode::End => area.end(),
        _ => {
            if let KeyCode::Char(c) = typed.code
                && !typed
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            {
                area.insert(c);
            }
        }
    }
    None
}

impl NotesView {
    /// Номер выделенного пункта.
    pub fn selected_entry(&self) -> Option<usize> {
        self.notes.visible().get(self.list.selected()?).copied()
    }

    fn clamp(&mut self) {
        let len = self.notes.visible().len();
        self.list.select(match (len, self.list.selected()) {
            (0, _) => None,
            (_, None) => Some(0),
            (_, Some(i)) => Some(i.min(len - 1)),
        });
    }
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
    last_merge: Instant,
    /// Копии, которые не удалось слить, — о них уже сказано.
    merge_failed: HashSet<String>,
    /// Карта попаданий последнего кадра; позже нарисованное — выше.
    pub hits: Vec<(Rect, Hit)>,
    /// Ширина экрана в последнем кадре.
    pub width: u16,
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
                && !typed
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
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
            last_merge: Instant::now(),
            merge_failed: HashSet::new(),
            hits: Vec::new(),
            width: 0,
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
        let book = &self.book;
        self.board.assign_ids(|| book.new_id());
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
        self.lists_stamp = self.book.lists_stamp();
        if !self.shown.is_empty() {
            self.refresh_shown();
        }
    }

    /// Меняет реестр списков и сохраняет его; ошибка — в строку состояния.
    fn edit_registry(&mut self, edit: impl FnOnce(&mut Lists) -> anyhow::Result<()>) -> bool {
        let mut lists = self.registry.clone();
        let result = edit(&mut lists).and_then(|()| self.book.save_lists(&lists));
        self.lists_stamp = self.book.lists_stamp();
        match result {
            Ok(()) => {
                self.registry = lists;
                self.refresh_shown();
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
        let selected_gone = selected
            .get(&self.active)
            .is_some_and(|t| self.find_task(self.active, t).is_none());
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

    fn reload_notes(&mut self) {
        let Some(view) = &mut self.notes else {
            return;
        };
        let notes = match self.book.load_notes(&view.id) {
            Ok(notes) => notes,
            Err(e) => {
                self.status = Some(Status::Error(format!("Записи задачи: {e:#}")));
                return;
            }
        };
        view.stamp = self.book.notes_stamp(&view.id);
        // Пункт, который правится, — по его тексту до правки (или последнему сохранённому).
        let find = |text: &str| notes.visible().into_iter().find(|&i| notes.editable_text(i) == text);
        match &mut view.mode {
            NotesMode::Text(draft) => {
                if let NoteTarget::Entry(_) = draft.target {
                    // Пропал — при сохранении добавится заново, ничего не потеряется.
                    draft.target = find(&draft.saved).map_or(NoteTarget::Note, NoteTarget::Entry);
                }
            }
            NotesMode::Edit { target: target @ NoteTarget::Entry(_), original, .. } => {
                match find(original) {
                    Some(i) => *target = NoteTarget::Entry(i),
                    None => view.mode = NotesMode::List,
                }
            }
            NotesMode::ConfirmDelete => view.mode = NotesMode::List,
            _ => {}
        }
        view.notes = notes;
        view.clamp();
        let summary = view.notes.summary();
        if summary.any {
            self.summaries.insert(view.id.clone(), summary);
        } else {
            self.summaries.remove(&view.id);
        }
    }

    /// Можно ли править открытую страницу; если нет — говорит почему.
    fn editable(&mut self) -> bool {
        if self.page_error.is_some() {
            self.status = Some(Status::Error(
                "Страница не прочитана — правка отключена, исправьте файл".into(),
            ));
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
            Mode::Settings(_) => self.on_key_settings(press),
        }
    }

    /// Открыто ли поле ввода. Тогда отслеживание мыши выключается: в Termux тап по экрану
    /// снова вызывает клавиатуру, а не уходит программе.
    pub fn wants_text(&self) -> bool {
        let notes_field = self
            .notes
            .as_ref()
            .is_some_and(|v| matches!(v.mode, NotesMode::Edit { .. } | NotesMode::Text(Draft { confirm_close: false, .. })));
        let lists_field = matches!(self.mode, Mode::Lists { sub: ListsMode::Add(_) | ListsMode::Rename(_), .. });
        notes_field || (self.notes.is_none() && (lists_field || matches!(self.mode, Mode::Edit { .. } | Mode::Date { .. })))
    }

    /// Тап (щелчок) и свайп (прокрутка): что под пальцем — по карте попаданий кадра.
    pub fn on_mouse(&mut self, mouse: Mouse) {
        let point = Position::new(mouse.column, mouse.row);
        let hit = self
            .hits
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(point))
            .map(|&(_, hit)| hit);
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
        if let (Mode::Settings(selected), Hit::Setting(i)) = (&mut self.mode, hit) {
            *selected = i;
            self.toggle_setting(Setting::ALL[i]);
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
                    text.chars()
                        .filter(|&c| c == '\n' || !c.is_control())
                        .for_each(|c| area.insert(c));
                    return;
                }
                _ => None,
            },
            (None, Mode::Edit { input, .. } | Mode::Date { input, .. }) => Some(input),
            (None, _) => None,
        };
        if let Some(input) = input {
            text.chars()
                .filter(|c| !c.is_control())
                .for_each(|c| input.insert(c));
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
            Action::GoToDate => {
                self.mode = Mode::Date {
                    input: LineInput::default(),
                    purpose: DatePurpose::Go,
                }
            }
            Action::Add => {
                self.mode = Mode::Edit {
                    input: LineInput::default(),
                    index: None,
                }
            }
            Action::Edit => {
                if let (Some(index), Some(task)) = (self.selected(), self.selected_task()) {
                    self.mode = Mode::Edit {
                        input: LineInput::new(task.text.clone()),
                        index: Some(index),
                    };
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
            Action::Settings => self.mode = Mode::Settings(0),
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
            let task = self.board.remove(self.active, index);
            self.clamp_selection(self.active);
            self.save();
            if let Some(task) = task {
                self.archive_notes(&task);
            }
        }
    }

    /// Записи удалённой задачи — в `DELETED_NOTES.md`, если её копий больше нигде нет.
    fn archive_notes(&mut self, task: &Task) {
        match self.book.archive_notes(task, self.date, &clock::stamp()) {
            Ok(true) => {
                self.status = Some(Status::Info(format!(
                    "Записи задачи сохранены в {DELETED_NOTES}"
                )));
            }
            Ok(false) => {}
            Err(e) => self.status = Some(Status::Error(format!("Записи задачи: {e:#}"))),
        }
        self.summaries = self.book.summaries(&self.board);
    }

    fn on_key_settings(&mut self, press: Press) {
        let Mode::Settings(selected) = &mut self.mode else {
            return;
        };
        let last = Setting::ALL.len() - 1;
        match press.hotkey.code {
            KeyCode::Char('j') | KeyCode::Down => *selected = (*selected + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Char(' ' | 'x') | KeyCode::Enter => {
                let setting = Setting::ALL[*selected];
                self.toggle_setting(setting);
            }
            KeyCode::Esc | KeyCode::Char('q' | 'S') => self.mode = Mode::Normal,
            _ => {}
        }
    }

    fn toggle_setting(&mut self, setting: Setting) {
        let on = !setting.is_on(&self.config);
        let saved = match setting {
            Setting::Preview => self.config.set_preview(on),
        };
        if let Err(e) = saved {
            self.status = Some(Status::Error(format!("Настройки не сохранены: {e:#}")));
        }
    }

    /// `v`: превью видно только на широком экране — на узком говорим об этом.
    fn toggle_preview(&mut self) {
        self.toggle_setting(Setting::Preview);
        if self.status.is_none() {
            let what = if self.config.preview() { "включено" } else { "выключено" };
            let note = if self.config.preview() && self.width < crate::ui::PREVIEW_MIN {
                format!(" (видно на экране от {} колонок)", crate::ui::PREVIEW_MIN)
            } else {
                String::new()
            };
            self.status = Some(Status::Info(format!("Превью {what}{note}")));
        }
    }

    /// Данные превью для выделенной задачи; собираются заново, только если что-то поменялось.
    pub fn preview(&mut self) -> Option<&Preview> {
        let index = self.selected()?;
        let target = (self.date, self.active, index);
        if self.preview_target != Some(target) {
            self.preview_target = Some(target);
            self.preview_scroll = 0;
        }
        let id = self.selected_task()?.id.clone();
        let notes_stamp = id.as_deref().and_then(|id| self.book.notes_stamp(id));
        let key = (id, self.page_stamp, notes_stamp);
        if self.preview.as_ref().is_none_or(|p| p.key != key) {
            let (history, notes) = match &key.0 {
                Some(id) => (
                    self.book.history(id).unwrap_or_default(),
                    self.book.load_notes(id).unwrap_or_default(),
                ),
                None => (Vec::new(), Notes::default()),
            };
            self.preview = Some(Preview { key, history, notes });
        }
        self.preview.as_ref()
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

    /// Строки окна списков: те, что на экране, по порядку, затем архивные.
    pub fn list_rows(&self) -> Vec<ListId> {
        let mut rows = self.registry.active();
        rows.extend(self.registry.all().iter().filter(|d| d.archived).map(|d| d.id));
        rows
    }

    /// Куда перенести задачи удаляемого списка.
    pub fn delete_targets(&self, list: ListId) -> Vec<ListId> {
        self.registry.active().into_iter().filter(|&c| c != list).collect()
    }

    /// Окно списков. Состояние вынимается из `mode` на время обработки и кладётся обратно,
    /// если окно не закрыли.
    fn on_key_lists(&mut self, press: Press) {
        let Mode::Lists { row, sub } = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return;
        };
        let rows = self.list_rows();
        let mut row = row.min(rows.len() - 1);
        let list = rows[row];
        let archived = !self.registry.is_active(list);
        let code = press.hotkey.code;
        let sub = match sub {
            ListsMode::Browse => match (code, press.hotkey.modifiers.contains(KeyModifiers::SHIFT)) {
                (KeyCode::Up | KeyCode::Down, true) => {
                    let up = code == KeyCode::Up;
                    let mut moved = false;
                    self.edit_registry(|lists| {
                        moved = lists.shift(list, up);
                        Ok(())
                    });
                    if moved {
                        row = if up { row - 1 } else { row + 1 };
                    }
                    ListsMode::Browse
                }
                (KeyCode::Char('j') | KeyCode::Down, _) => {
                    row = (row + 1).min(rows.len() - 1);
                    ListsMode::Browse
                }
                (KeyCode::Char('k') | KeyCode::Up, _) => {
                    row = row.saturating_sub(1);
                    ListsMode::Browse
                }
                (KeyCode::Char('a'), _) => ListsMode::Add(LineInput::default()),
                (KeyCode::Char('e') | KeyCode::Enter, _) if !archived => {
                    ListsMode::Rename(LineInput::new(self.list_title(list).to_string()))
                }
                (KeyCode::Char('r'), _) if archived => {
                    if self.edit_registry(|lists| lists.restore(list)) {
                        row = self.registry.active().len() - 1;
                        self.status = Some(Status::Info(format!("Список «{}» снова на экране", self.list_title(list))));
                    }
                    ListsMode::Browse
                }
                (KeyCode::Char('d') | KeyCode::Delete, _) if !archived => self.start_delete(list),
                (KeyCode::Char('p' | 'P'), _) => {
                    self.toggle_hidden_of(list);
                    ListsMode::Browse
                }
                (KeyCode::Esc | KeyCode::Char('q' | 'L'), _) => return,
                _ => ListsMode::Browse,
            },
            ListsMode::Add(mut input) => match edit_line(&mut input, &press) {
                Some(KeyCode::Enter) => {
                    let id = ListId::new(&self.book.new_id());
                    let title = input.text().to_string();
                    if self.edit_registry(|lists| lists.add(id, &title)) {
                        row = self.registry.active().len() - 1;
                        self.status = Some(Status::Info(format!("Список «{}» добавлен", self.list_title(id))));
                        ListsMode::Browse
                    } else {
                        ListsMode::Add(input)
                    }
                }
                Some(_) => ListsMode::Browse,
                None => ListsMode::Add(input),
            },
            ListsMode::Rename(mut input) => match edit_line(&mut input, &press) {
                Some(KeyCode::Enter) => {
                    let title = input.text().to_string();
                    if self.edit_registry(|lists| lists.rename(list, &title)) {
                        ListsMode::Browse
                    } else {
                        ListsMode::Rename(input)
                    }
                }
                Some(_) => ListsMode::Browse,
                None => ListsMode::Rename(input),
            },
            ListsMode::Delete { count: 0, .. } => {
                if matches!(code, KeyCode::Char('y') | KeyCode::Enter) {
                    self.finish_delete(list, None);
                    row = row.min(self.list_rows().len() - 1);
                }
                ListsMode::Browse
            }
            ListsMode::Delete { count, mut target } => {
                let targets = self.delete_targets(list);
                let to = match code {
                    KeyCode::Char(c @ '1'..='9') => targets.get(c as usize - '1' as usize).copied(),
                    KeyCode::Enter => targets.get(target).copied(),
                    KeyCode::Char('j') | KeyCode::Down => {
                        target = (target + 1).min(targets.len() - 1);
                        None
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        target = target.saturating_sub(1);
                        None
                    }
                    _ => {
                        self.mode = Mode::Lists { row, sub: ListsMode::Browse };
                        return;
                    }
                };
                match to {
                    Some(to) => {
                        self.finish_delete(list, Some(to));
                        row = row.min(self.list_rows().len() - 1);
                        ListsMode::Browse
                    }
                    None => ListsMode::Delete { count, target },
                }
            }
        };
        self.mode = Mode::Lists { row, sub };
    }

    /// `d` в окне списков: последний не удаляется; есть задачи на сегодня и дальше — сначала
    /// выбрать, куда их перенести.
    fn start_delete(&mut self, list: ListId) -> ListsMode {
        if self.registry.active().len() == 1 {
            self.status = Some(Status::Error("Последний список удалить нельзя".into()));
            return ListsMode::Browse;
        }
        match self.book.tasks_since(list, self.today) {
            Ok(count) => ListsMode::Delete { count, target: 0 },
            Err(e) => {
                self.status = Some(Status::Error(format!("Удаление списка: {e:#}")));
                ListsMode::Browse
            }
        }
    }

    /// Удаление списка: задачи с сегодняшней страницы и дальше — в `to`; если задачи в нём
    /// когда-нибудь были, он уходит в архив (прошлые страницы показывают его как было),
    /// иначе удаляется совсем.
    fn finish_delete(&mut self, list: ListId, to: Option<ListId>) {
        let title = self.list_title(list).to_string();
        let moved = match to.map(|to| self.book.move_list_tasks(list, to, self.today)) {
            Some(Err(e)) => {
                self.status = Some(Status::Error(format!("Удаление списка: {e:#}")));
                return;
            }
            Some(Ok(n)) => n,
            None => 0,
        };
        let used = moved > 0 || self.book.list_used(list).unwrap_or(true);
        if !self.edit_registry(|lists| if used { lists.archive(list) } else { lists.remove(list) }) {
            return;
        }
        self.reload_page();
        self.status = Some(Status::Info(match to {
            Some(to) if moved > 0 => format!("Список «{title}» удалён, задачи ({moved}) — в «{}»", self.list_title(to)),
            _ if used => format!("Список «{title}» удалён; на прошлых страницах он остался"),
            _ => format!("Список «{title}» удалён"),
        }));
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
        self.mode = Mode::Date {
            input: LineInput::new(default.to_string()),
            purpose: DatePurpose::Postpone(index),
        };
    }

    fn postpone(&mut self, index: usize, to: Date) {
        if to == self.date {
            self.status = Some(Status::Info("Это та же страница".into()));
            return;
        }
        match self
            .book
            .postpone(&mut self.board, self.date, self.active, index, to)
        {
            Ok(true) => {
                self.page_stamp = self.book.page_stamp(self.date);
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
                let new_index = self.board.push(self.active, Task::new(text));
                self.select(self.active, Some(new_index));
            }
            Some(index) => match self.board.get_mut(self.active, index) {
                Some(task) => task.text = text,
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
        self.status = Some(Status::Info(format!(
            "Задача перенесена на {when} — отметьте её там"
        )));
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

/// Окно доп. записей.
impl App {
    fn open_notes(&mut self) {
        let Some(index) = self.selected() else {
            return;
        };
        let task = &self.board.list(self.active)[index];
        let id = task.id.clone().unwrap_or_else(|| self.book.new_id());
        let stamp = self.book.notes_stamp(&id);
        match self.book.load_notes(&id) {
            Ok(notes) => {
                let mut view = NotesView {
                    category: self.active,
                    index,
                    id,
                    notes,
                    list: ListState::default(),
                    mode: NotesMode::List,
                    stamp,
                };
                view.clamp();
                self.notes = Some(view);
            }
            Err(e) => self.status = Some(Status::Error(format!("Записи задачи: {e:#}"))),
        }
    }

    /// Задача, чьи записи открыты.
    pub fn notes_task(&self) -> Option<&Task> {
        let view = self.notes.as_ref()?;
        self.board.list(view.category).get(view.index)
    }

    fn on_key_notes(&mut self, press: Press) {
        let Some(view) = &mut self.notes else {
            return;
        };
        match &mut view.mode {
            NotesMode::List => {
                let key = (press.hotkey.code, press.hotkey.modifiers);
                if let Some(action) = keymap::notes_action(key) {
                    self.act_notes(action);
                }
            }
            NotesMode::Edit { input, target, .. } => match edit_line(input, &press) {
                Some(KeyCode::Enter) => {
                    let (text, target) = (input.text().trim().to_string(), *target);
                    self.commit_note(target, &text);
                }
                Some(_) => view.mode = NotesMode::List,
                None => {}
            },
            NotesMode::Text(draft) if draft.confirm_close => {
                if ctrl(&press, 's') || press.hotkey.code == KeyCode::Char('s') {
                    self.save_draft(true);
                    self.close_draft();
                } else if matches!(press.hotkey.code, KeyCode::Char('y') | KeyCode::Enter) {
                    self.close_draft();
                } else {
                    draft.confirm_close = false;
                }
            }
            NotesMode::Text(draft) => match edit_area(&mut draft.area, &press) {
                Some(Done::Save) => {
                    self.save_draft(true);
                    self.close_draft();
                }
                Some(Done::Cancel) if draft.is_changed() => draft.confirm_close = true,
                Some(Done::Cancel) => self.close_draft(),
                None => {}
            },
            NotesMode::ConfirmDelete => {
                view.mode = NotesMode::List;
                if matches!(press.hotkey.code, KeyCode::Char('y') | KeyCode::Enter)
                    && let Some(entry) = view.selected_entry()
                {
                    self.remove_entry(entry);
                }
            }
        }
    }

    /// Сохраняет многострочную заметку, если есть что: новая при первом сохранении
    /// добавляется в конец записей, дальше — обновляется на месте. Стёртый весь текст
    /// удаляет заметку, но только при явном сохранении (`explicit`): автосохранение и
    /// сохранение при выходе пустоту не пишут — вдруг текст стёрли, чтобы набрать заново.
    fn save_draft(&mut self, explicit: bool) {
        let Some(view) = &mut self.notes else {
            return;
        };
        let NotesMode::Text(draft) = &mut view.mode else {
            return;
        };
        if !draft.is_changed() {
            return;
        }
        if draft.is_empty() {
            if explicit && let NoteTarget::Entry(entry) = draft.target {
                draft.target = NoteTarget::Note;
                draft.saved = String::new();
                self.remove_entry(entry);
            }
            return;
        }
        let text = normalize(draft.area.text());
        let added = match draft.target {
            NoteTarget::Entry(entry) => {
                view.notes.set_text(entry, &text);
                false
            }
            NoteTarget::Note | NoteTarget::Subtask => {
                draft.target = NoteTarget::Entry(view.notes.add_note(&draft.stamp, &text));
                true
            }
        };
        draft.saved = text;
        draft.saved_at = Instant::now();
        draft.saved_label = Some(clock::time());
        if added {
            view.list.select(Some(usize::MAX));
            view.clamp();
        }
        self.save_notes();
    }

    /// Удаляет пункт записей; как и всё удалённое из записей, он уходит в `DELETED_NOTES.md`.
    fn remove_entry(&mut self, entry: usize) {
        let task = self.notes_task().map(|t| t.text.clone()).unwrap_or_default();
        let Some(view) = &mut self.notes else {
            return;
        };
        let (raw, id) = (view.notes.raw(entry), view.id.clone());
        view.notes.remove(entry);
        view.clamp();
        if let Err(e) = self.book.archive_entry(&task, &id, self.date, &clock::stamp(), &raw) {
            self.status = Some(Status::Error(format!("Архив записей: {e:#}")));
            return;
        }
        self.save_notes();
    }

    /// Закрывает многострочное поле; сохранённое остаётся как есть.
    fn close_draft(&mut self) {
        if let Some(view) = &mut self.notes {
            view.mode = NotesMode::List;
        }
    }

    /// Ход часов: автосохранение заметки и слияние копий с других устройств. Возвращает,
    /// было ли что (перерисовать).
    pub fn tick(&mut self, now: Instant) -> bool {
        let merged = now.duration_since(self.last_merge) >= MERGE_EVERY && {
            self.last_merge = now;
            self.merge_conflicts()
        };
        let due = match &self.notes {
            Some(NotesView { mode: NotesMode::Text(draft), .. }) => {
                draft.is_changed()
                    && !draft.is_empty()
                    && now.duration_since(draft.saved_at) >= AUTOSAVE
            }
            _ => false,
        };
        if due {
            self.save_draft(false);
        }
        due || merged
    }

    /// Сливает конфликтные копии Syncthing (сами файлы потом подхватит `check_external`).
    /// Возвращает, было ли что сообщить.
    pub fn merge_conflicts(&mut self) -> bool {
        match self.book.merge_conflicts() {
            Ok(report) => {
                // О неслитой копии — один раз, а не каждые две секунды.
                let failed: Vec<String> = report
                    .failed
                    .into_iter()
                    .filter(|f| self.merge_failed.insert(f.clone()))
                    .collect();
                if !failed.is_empty() {
                    self.status = Some(Status::Error(format!(
                        "Не удалось слить копии с другого устройства (исправьте вручную): {}",
                        failed.join(", ")
                    )));
                } else if !report.merged.is_empty() {
                    self.status = Some(Status::Info(format!(
                        "Слиты изменения с другого устройства: {}",
                        report.merged.join(", ")
                    )));
                } else {
                    return false;
                }
                true
            }
            Err(e) => {
                self.status = Some(Status::Error(format!("Слияние копий: {e:#}")));
                true
            }
        }
    }

    /// Закрывает однострочное поле окна записей и сохраняет введённое. Пустое у нового
    /// пункта — отмена, у существующего — удаление пункта.
    fn commit_note(&mut self, target: NoteTarget, text: &str) {
        let Some(view) = &mut self.notes else {
            return;
        };
        view.mode = NotesMode::List;
        if text.trim().is_empty() {
            if let NoteTarget::Entry(entry) = target {
                self.remove_entry(entry);
            }
            return;
        }
        match target {
            NoteTarget::Note => {
                view.notes.add_note(&clock::stamp(), text);
            }
            NoteTarget::Subtask => view.notes.add_subtask(text),
            NoteTarget::Entry(entry) => view.notes.set_text(entry, text),
        }
        // Новый пункт — в конце, выделяем его.
        if !matches!(target, NoteTarget::Entry(_)) {
            view.list.select(Some(usize::MAX));
            view.clamp();
        }
        self.save_notes();
    }

    fn act_notes(&mut self, action: Action) {
        let Some(view) = &mut self.notes else {
            return;
        };
        let selected = view.selected_entry();
        match action {
            Action::Close => self.notes = None,
            Action::Help => self.show_help = true,
            Action::Up => {
                view.list.select_previous();
                view.clamp();
            }
            Action::Down => {
                view.list.select_next();
                view.clamp();
            }
            Action::Top => {
                view.list.select(Some(0));
                view.clamp();
            }
            Action::Bottom => {
                view.list.select(Some(usize::MAX));
                view.clamp();
            }
            Action::Add => view.mode = NotesMode::Text(Draft::new(String::new(), NoteTarget::Note)),
            Action::AddSubtask => {
                view.mode = NotesMode::Edit {
                    input: LineInput::default(),
                    target: NoteTarget::Subtask,
                    original: String::new(),
                };
            }
            Action::Edit => {
                if let Some(entry) = selected {
                    let text = view.notes.editable_text(entry);
                    let target = NoteTarget::Entry(entry);
                    view.mode = if view.notes.is_multiline(entry) {
                        NotesMode::Text(Draft::new(text, target))
                    } else {
                        NotesMode::Edit {
                            input: LineInput::new(text.clone()),
                            target,
                            original: text,
                        }
                    };
                }
            }
            Action::Toggle => {
                if let Some(line) = selected
                    && view.notes.toggle(line)
                {
                    self.save_notes();
                }
            }
            Action::MoveUp | Action::MoveDown => {
                let visible = view.notes.visible();
                let Some(i) = view.list.selected() else {
                    return;
                };
                let j = if action == Action::MoveUp {
                    i.checked_sub(1)
                } else {
                    Some(i + 1).filter(|&j| j < visible.len())
                };
                if let Some(j) = j {
                    view.notes.swap(visible[i], visible[j]);
                    view.list.select(Some(j));
                    self.save_notes();
                }
            }
            Action::Delete => {
                if selected.is_some() {
                    view.mode = NotesMode::ConfirmDelete;
                }
            }
            Action::OpenEditor => match self.book.notes_file(&view.id) {
                Ok(path) => self.editor_request = Some(path),
                Err(e) => self.status = Some(Status::Error(format!("Записи задачи: {e:#}"))),
            },
            _ => {}
        }
    }

    /// Сохраняет записи открытого окна. Задача без id получает его с первой записью
    /// (и страница сохраняется); пустые записи — без файла.
    fn save_notes(&mut self) {
        let Some(view) = &self.notes else {
            return;
        };
        let (id, category, index) = (view.id.clone(), view.category, view.index);
        let summary = view.notes.summary();
        if let Err(e) = self.book.save_notes(&id, &view.notes) {
            self.status = Some(Status::Error(format!("Записи задачи: {e:#}")));
            return;
        }
        let stamp = self.book.notes_stamp(&id);
        if let Some(view) = &mut self.notes {
            view.stamp = stamp;
        }
        if summary.any {
            self.summaries.insert(id.clone(), summary);
        } else {
            self.summaries.remove(&id);
        }
        if let Some(task) = self.board.get_mut(category, index)
            && task.id.is_none()
            && summary.any
        {
            task.id = Some(id);
            self.save();
        }
    }

    /// Редактор закрылся (`error` — если не запустился): перечитать записи.
    pub fn after_editor(&mut self, error: Option<String>) {
        if let Some(e) = error {
            self.status = Some(Status::Error(e));
        }
        let Some(view) = &mut self.notes else {
            return;
        };
        match self.book.load_notes(&view.id) {
            Ok(notes) => {
                view.notes = notes;
                view.clamp();
                // Пустой файл из редактора удаляется, непустой — привязывается к задаче.
                self.save_notes();
            }
            Err(e) => self.status = Some(Status::Error(format!("Записи задачи: {e:#}"))),
        }
    }
}

pub fn carried_message(from: Date, count: usize) -> String {
    format!(
        "Перенесено невыполненных задач со страницы {}: {count}",
        from.format(c"%-d %B")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    fn d(day: u32) -> Date {
        Date::new(2026, 10, day).unwrap()
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(code);
    }

    fn press_with(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        app.on_key(KeyEvent::new(code, mods));
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    /// Ctrl+S — сохранить многострочное поле.
    fn save_note(app: &mut App) {
        press_with(app, KeyCode::Char('s'), KeyModifiers::CONTROL);
    }

    fn test_app(name: &str) -> App {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        App::new(Book::new(dir), Config::default(), d(26))
    }

    fn cleanup(app: App) {
        let _ = std::fs::remove_dir_all(app.book.dir());
    }

    fn texts(app: &App, category: ListId) -> Vec<String> {
        app.board.list(category).iter().map(|t| t.text.clone()).collect()
    }

    #[test]
    fn add_move_toggle_delete() {
        let mut app = test_app("flow");

        for text in ["первая", "вторая"] {
            press(&mut app, KeyCode::Char('a'));
            type_text(&mut app, text);
            press(&mut app, KeyCode::Enter);
        }
        assert_eq!(texts(&app, ListId::WORK), ["первая", "вторая"]);
        assert_eq!(app.selected(), Some(1));

        press_with(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(texts(&app, ListId::WORK), ["вторая", "первая"]);
        assert_eq!(app.selected(), Some(0));
        // J/K больше не сдвигают.
        press(&mut app, KeyCode::Char('J'));
        assert_eq!(texts(&app, ListId::WORK), ["вторая", "первая"]);

        // Выполненная — вниз, выделение — на следующей задаче.
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(texts(&app, ListId::WORK), ["первая", "вторая"]);
        assert_eq!(app.board.list(ListId::WORK)[1].state, State::Done);
        assert_eq!(app.selected(), Some(0));
        // Снятая отметка — в конец открытых, выделение за ней.
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.board.list(ListId::WORK)[1].state, State::Open);
        assert_eq!(app.selected(), Some(1));
        press(&mut app, KeyCode::Char(' '));

        press(&mut app, KeyCode::Char('j'));
        // m — окно выбора без текущего списка: Дом, Личное, Прочее.
        press(&mut app, KeyCode::Char('m'));
        assert!(matches!(app.mode, Mode::MoveTo(0)));
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(texts(&app, ListId::WORK), ["первая"]);
        assert_eq!(texts(&app, ListId::PERSONAL), ["вторая"]);
        assert_eq!(app.lists[&ListId::PERSONAL].selected(), Some(0));

        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(texts(&app, ListId::WORK), ["первая"]);
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));
        assert!(app.board.list(ListId::WORK).is_empty());
        assert_eq!(app.selected(), None);

        assert_eq!(app.book.load(d(26)).unwrap(), app.board);
        assert_eq!(app.status, None);
        cleanup(app);
    }

    #[test]
    fn cancel() {
        let mut app = test_app("cancel");
        for text in ["раз", "два"] {
            press(&mut app, KeyCode::Char('a'));
            type_text(&mut app, text);
            press(&mut app, KeyCode::Enter);
        }
        press(&mut app, KeyCode::Char('g'));
        // Отменённая — вниз, выделение на следующей.
        press(&mut app, KeyCode::Char('c'));
        assert_eq!(texts(&app, ListId::WORK), ["два", "раз"]);
        assert_eq!(app.board.list(ListId::WORK)[1].state, State::Cancelled);
        assert_eq!(app.selected(), Some(0));
        assert_eq!(app.book.load(d(26)).unwrap(), app.board);
        // Повторно — снова открыта, выделение за ней; «с» в русской раскладке — та же клавиша.
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('с'));
        assert!(app.board.list(ListId::WORK)[1].is_open());
        assert_eq!(app.selected(), Some(1));
        // Отменённую всё-таки сделали.
        press(&mut app, KeyCode::Char('c'));
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.board.list(ListId::WORK)[1].state, State::Done);
        // Отменённая не переносится.
        press(&mut app, KeyCode::Char('c'));
        press(&mut app, KeyCode::Char('>'));
        assert!(matches!(app.mode, Mode::Normal));
        cleanup(app);
    }

    #[test]
    fn russian_layout() {
        let mut app = test_app("ru");
        // «ф» — физическая a: новая задача; в поле ввода — сама «ф».
        press(&mut app, KeyCode::Char('ф'));
        type_text(&mut app, "фото");
        press(&mut app, KeyCode::Enter);
        assert_eq!(texts(&app, ListId::WORK), ["фото"]);
        // «ч» — x: выполнено; «в» + «н» — d + y: удалить.
        press(&mut app, KeyCode::Char('ч'));
        assert_eq!(app.board.list(ListId::WORK)[0].state, State::Done);
        press(&mut app, KeyCode::Char('в'));
        press(&mut app, KeyCode::Char('н'));
        assert!(app.board.list(ListId::WORK).is_empty());
        // Ctrl+W в поле ввода в русской раскладке.
        press(&mut app, KeyCode::Char('ф'));
        type_text(&mut app, "два слова");
        press_with(&mut app, KeyCode::Char('ц'), KeyModifiers::CONTROL);
        let Mode::Edit { input, .. } = &app.mode else { panic!() };
        assert_eq!(input.text(), "два ");
        // «х» / «ъ» — [ / ]: листание.
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('ъ'));
        assert_eq!(app.date, d(27));
        cleanup(app);
    }

    #[test]
    fn edit_and_cancel() {
        let mut app = test_app("edit");
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "кран");
        press(&mut app, KeyCode::Enter);

        press(&mut app, KeyCode::Enter);
        type_text(&mut app, " течёт");
        press(&mut app, KeyCode::Esc);
        assert_eq!(texts(&app, ListId::HOME), ["кран"]);

        press(&mut app, KeyCode::Char('e'));
        press(&mut app, KeyCode::Home);
        app.on_paste("Починить\n ");
        press(&mut app, KeyCode::Enter);
        assert_eq!(texts(&app, ListId::HOME), ["Починить кран"]);

        // Пустой ввод не создаёт задачу.
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "   ");
        press(&mut app, KeyCode::Enter);
        assert_eq!(texts(&app, ListId::HOME).len(), 1);
        cleanup(app);
    }

    #[test]
    fn pages_and_postpone() {
        let mut app = test_app("pages");
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "позвонить");
        press(&mut app, KeyCode::Enter);

        // Перенос: по умолчанию на завтра.
        press(&mut app, KeyCode::Char('>'));
        let Mode::Date { input, .. } = &app.mode else { panic!() };
        assert_eq!(input.text(), "2026-10-27");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.board.list(ListId::WORK)[0].state, State::Migrated(d(27)));
        assert!(matches!(app.status, Some(Status::Info(_))));
        // Перенесённую не отметить и не перенести второй раз.
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.board.list(ListId::WORK)[0].state, State::Migrated(d(27)));
        press(&mut app, KeyCode::Char('>'));
        assert!(matches!(app.mode, Mode::Normal));

        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.date, d(27));
        assert_eq!(texts(&app, ListId::WORK), ["позвонить"]);
        assert!(app.board.list(ListId::WORK)[0].is_open());

        // Переход к дате: не дата — окно остаётся открытым.
        press(&mut app, KeyCode::Char('D'));
        type_text(&mut app, "31.02");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Date { .. }));
        press_with(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        type_text(&mut app, "+3");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.date, d(30));
        assert!(app.board.is_empty());

        // Все задачи удалены — файла страницы нет.
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.date, d(26));
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.book.dates().unwrap(), [d(27)]);
        cleanup(app);
    }

    #[test]
    fn broken_page_is_read_only() {
        let app = test_app("broken");
        let path = app.book.dir().join("2026-10-25.md");
        std::fs::create_dir_all(app.book.dir()).unwrap();
        std::fs::write(&path, "## Работа\nпросто текст\n").unwrap();
        let mut app = app;
        press(&mut app, KeyCode::Char('['));
        assert!(app.page_error.is_some());
        press(&mut app, KeyCode::Char('a'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(matches!(app.status, Some(Status::Error(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "## Работа\nпросто текст\n");
        press(&mut app, KeyCode::Char(']'));
        assert!(app.page_error.is_none() && app.status.is_none());
        cleanup(app);
    }

    #[test]
    fn new_day_carries_over() {
        let mut app = test_app("midnight");
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "недоделано");
        press(&mut app, KeyCode::Enter);
        app.on_new_day(d(27));
        assert_eq!(app.date, d(27));
        assert_eq!(texts(&app, ListId::WORK), ["недоделано"]);
        assert!(matches!(app.status, Some(Status::Info(_))));
        press(&mut app, KeyCode::Char('['));
        assert_eq!(app.board.list(ListId::WORK)[0].state, State::Migrated(d(27)));
        cleanup(app);
    }

    #[test]
    fn notes_window() {
        let mut app = test_app("notes");
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "кран");
        press(&mut app, KeyCode::Enter);

        // id у задачи с самого начала; открыли и закрыли без записей — файла записей нет.
        let first_id = app.board.list(ListId::HOME)[0].id.clone().expect("id при создании");
        press(&mut app, KeyCode::Char('n'));
        assert!(app.notes.is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.notes.is_none());
        assert!(app.book.load_notes(&first_id).unwrap().is_empty());
        assert!(app.summaries.is_empty());

        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Char('s'));
        type_text(&mut app, "купить прокладку");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('a'));
        app.on_paste("мастер сказал");
        save_note(&mut app);
        // Горячие клавиши окна не трогают страницу: «x» — подзадача, а не задача.
        press(&mut app, KeyCode::Char('k'));
        press(&mut app, KeyCode::Char('x'));

        let id = app.board.list(ListId::HOME)[0].id.clone().unwrap();
        assert_eq!(id, first_id);
        assert_eq!(app.board.list(ListId::HOME)[0].state, State::Open);
        let notes = app.book.load_notes(&id).unwrap().serialize();
        let lines: Vec<&str> = notes.lines().collect();
        assert_eq!(lines[0], "- [x] купить прокладку");
        // Метка заметки — по настоящим часам, а не по открытой странице.
        let stamp = format!("- {}", Date::today().format(c"%d.%m.%Y"));
        assert!(lines[1].starts_with(&stamp) && lines[1].ends_with(" мастер сказал"), "{notes}");
        assert_eq!(app.summaries[&id].subtasks_done, 1);
        // id сохранён на странице.
        assert_eq!(app.book.load(d(26)).unwrap(), app.board);

        // Сдвиг и правка строки.
        press_with(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
        press(&mut app, KeyCode::Char('e'));
        press_with(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        type_text(&mut app, "смеситель");
        press(&mut app, KeyCode::Enter);
        let notes = app.book.load_notes(&id).unwrap().serialize();
        assert!(notes.ends_with("- [x] купить смеситель\n"), "{notes}");

        // Удаление строки с подтверждением.
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));
        assert!(app.notes.as_ref().unwrap().notes.is_empty());
        assert!(!app.summaries.contains_key(&id));
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.quit);
        cleanup(app);
    }

    #[test]
    fn notes_follow_postpone_and_archive_on_delete() {
        let mut app = test_app("archive");
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "отчёт");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "начал");
        save_note(&mut app);
        press(&mut app, KeyCode::Esc);
        let id = app.board.list(ListId::WORK)[0].id.clone().unwrap();

        // Перенос — копия с теми же записями.
        press(&mut app, KeyCode::Char('>'));
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char(']'));
        assert_eq!(app.board.list(ListId::WORK)[0].id.as_deref(), Some(id.as_str()));
        assert!(app.summaries.contains_key(&id));

        // Удалили копию — запись [>] на 26-м осталась, записи на месте.
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));
        assert!(app.book.load_notes(&id).unwrap().summary().any);
        // Удалили и её — записи ушли в DELETED_NOTES.md.
        press(&mut app, KeyCode::Char('['));
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));
        assert!(app.book.load_notes(&id).unwrap().is_empty());
        let deleted = std::fs::read_to_string(app.book.dir().join(DELETED_NOTES)).unwrap();
        assert!(deleted.contains("— отчёт\n") && deleted.contains(" начал\n"), "{deleted}");
        assert!(matches!(app.status, Some(Status::Info(_))));
        cleanup(app);
    }

    #[test]
    fn multiline_note() {
        let mut app = test_app("multiline");
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "смеситель");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Char('a'));
        // Enter — новая строка, пустые строки по краям отбрасываются.
        press(&mut app, KeyCode::Enter);
        type_text(&mut app, "мастер сказал:");
        press(&mut app, KeyCode::Enter);
        type_text(&mut app, "менять целиком");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        app.on_paste("и ещё\r\nдве строки\n");
        press(&mut app, KeyCode::Enter);
        save_note(&mut app);

        let view = app.notes.as_ref().unwrap();
        assert!(matches!(view.mode, NotesMode::List));
        let item = view.notes.item(0);
        assert_eq!(item.head, "мастер сказал:");
        assert_eq!(item.body, ["менять целиком", "", "и ещё", "две строки"]);
        let id = app.board.list(ListId::WORK)[0].id.clone().unwrap();
        let file = app.book.load_notes(&id).unwrap().serialize();
        assert!(file.ends_with(" мастер сказал:\n  менять целиком\n\n  и ещё\n  две строки\n"), "{file}");

        // Правка — в многострочном поле с тем же текстом; Esc — отмена.
        press(&mut app, KeyCode::Char('e'));
        let Some(NotesView { mode: NotesMode::Text(Draft { area, .. }), .. }) = &app.notes else {
            panic!()
        };
        assert_eq!(area.text(), "мастер сказал:\nменять целиком\n\nи ещё\nдве строки");
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::End);
        type_text(&mut app, "!");
        save_note(&mut app);
        assert_eq!(app.notes.as_ref().unwrap().notes.item(0).body[2], "и ещё!");

        // Подзадача — однострочная: Enter сохраняет.
        press(&mut app, KeyCode::Char('s'));
        type_text(&mut app, "купить");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.notes.as_ref().unwrap().notes.visible().len(), 2);
        press(&mut app, KeyCode::Char('e'));
        assert!(matches!(app.notes.as_ref().unwrap().mode, NotesMode::Edit { .. }));
        press(&mut app, KeyCode::Esc);
        // Пустая заметка — не сохраняется.
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Enter);
        save_note(&mut app);
        assert_eq!(app.notes.as_ref().unwrap().notes.visible().len(), 2);
        cleanup(app);
    }

    /// Текст первой заметки в файле записей задачи из «Работы».
    fn saved_note(app: &App) -> Option<String> {
        let id = app.board.list(ListId::WORK)[0].id.clone()?;
        let notes = app.book.load_notes(&id).unwrap();
        notes.visible().first().map(|&i| notes.editable_text(i))
    }

    fn draft(app: &App) -> &Draft {
        match &app.notes.as_ref().unwrap().mode {
            NotesMode::Text(draft) => draft,
            _ => panic!("ожидалось многострочное поле"),
        }
    }

    #[test]
    fn note_autosave_and_close() {
        let mut app = test_app("autosave");
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "задача");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "черновик");

        // Раньше 10 секунд — не сохраняется, потом — сохраняется сама.
        assert!(!app.tick(Instant::now()));
        assert_eq!(saved_note(&app), None);
        assert!(app.tick(Instant::now() + AUTOSAVE));
        assert_eq!(saved_note(&app).as_deref(), Some("черновик"));
        assert!(draft(&app).saved_label.is_some() && !draft(&app).is_changed());

        // Дописали — Esc спрашивает; «другая клавиша» возвращает к правке.
        type_text(&mut app, " ещё");
        press(&mut app, KeyCode::Esc);
        assert!(draft(&app).confirm_close);
        press(&mut app, KeyCode::Char('x'));
        assert!(!draft(&app).confirm_close);
        assert_eq!(draft(&app).area.text(), "черновик ещё");

        // «y» — закрыть без последних изменений; сохранённое остаётся.
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('y'));
        assert!(matches!(app.notes.as_ref().unwrap().mode, NotesMode::List));
        assert_eq!(saved_note(&app).as_deref(), Some("черновик"));
        // Заметка одна: автосохранение обновляет её, а не добавляет новую.
        assert_eq!(app.notes.as_ref().unwrap().notes.visible().len(), 1);

        // «s» в вопросе — сохранить и закрыть.
        press(&mut app, KeyCode::Char('e'));
        type_text(&mut app, "!");
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('s'));
        assert!(matches!(app.notes.as_ref().unwrap().mode, NotesMode::List));
        assert_eq!(saved_note(&app).as_deref(), Some("черновик!"));

        // Без изменений Esc закрывает сразу.
        press(&mut app, KeyCode::Char('e'));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.notes.as_ref().unwrap().mode, NotesMode::List));

        // Стёрли всё: автосохранение и Esc + «y» заметку не трогают…
        press(&mut app, KeyCode::Char('e'));
        press_with(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert!(draft(&app).is_empty());
        assert!(!app.tick(Instant::now() + AUTOSAVE));
        press(&mut app, KeyCode::Esc);
        assert!(draft(&app).confirm_close);
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(saved_note(&app).as_deref(), Some("черновик!"));
        // …а Ctrl+S — удаляет. Подзадача, стёртая и сохранённая Enter, — тоже.
        press(&mut app, KeyCode::Char('s'));
        type_text(&mut app, "пункт");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('g'));
        press(&mut app, KeyCode::Char('e'));
        press_with(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        save_note(&mut app);
        assert!(matches!(app.notes.as_ref().unwrap().mode, NotesMode::List));
        let notes = &app.notes.as_ref().unwrap().notes;
        assert_eq!(notes.visible().len(), 1);
        assert_eq!(notes.editable_text(notes.visible()[0]), "пункт");
        press(&mut app, KeyCode::Char('e'));
        press_with(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Enter);
        assert!(app.notes.as_ref().unwrap().notes.is_empty());
        // Файла записей нет, значка у задачи нет.
        assert_eq!(saved_note(&app), None);
        assert!(app.summaries.is_empty());

        // Ctrl+C с начатой заметкой — сначала сохранить.
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "черновик!");
        save_note(&mut app);
        press(&mut app, KeyCode::Char('e'));
        type_text(&mut app, "?");
        press_with(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.quit);
        assert_eq!(saved_note(&app).as_deref(), Some("черновик!?"));
        cleanup(app);
    }

    #[test]
    fn lists_window() {
        let mut app = test_app("lists");
        // L, a — новый список в конец; его клавиша — 5.
        press(&mut app, KeyCode::Char('L'));
        press(&mut app, KeyCode::Char('a'));
        assert!(app.wants_text());
        type_text(&mut app, "Ремонт");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.shown.len(), 5);
        let repair = app.shown[4];
        assert_eq!(app.list_title(repair), "Ремонт");
        assert!(matches!(app.mode, Mode::Lists { row: 4, sub: ListsMode::Browse }));
        // Занятое название — ошибка, поле остаётся открытым.
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "дом");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.status, Some(Status::Error(_))));
        assert!(matches!(app.mode, Mode::Lists { sub: ListsMode::Add(_), .. }));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));

        // Задачи в новом списке: сегодня и на прошлой странице.
        press(&mut app, KeyCode::Char('5'));
        assert_eq!(app.active, repair);
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "плитка");
        press(&mut app, KeyCode::Enter);
        let mut past = Board::default();
        past.push(repair, Task::new("замер"));
        app.book.save(d(25), &past).unwrap();

        // Переименовать, сдвинуть выше.
        press(&mut app, KeyCode::Char('L'));
        press(&mut app, KeyCode::Char('e'));
        press_with(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        type_text(&mut app, "Ванная");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.list_title(repair), "Ванная");
        press_with(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(app.registry.active()[3], repair);
        assert!(matches!(app.mode, Mode::Lists { row: 3, .. }));

        // Удалить: задачу с сегодняшней страницы — в выбранный список, сам список — в архив.
        press(&mut app, KeyCode::Char('d'));
        assert!(matches!(app.mode, Mode::Lists { sub: ListsMode::Delete { count: 1, .. }, .. }));
        press(&mut app, KeyCode::Char('1'));
        assert!(!app.registry.is_active(repair) && app.registry.get(repair).is_some());
        assert_eq!(texts(&app, ListId::WORK), ["плитка"]);
        assert!(!app.shown.contains(&repair));
        // На прошлой странице список виден как был.
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('['));
        assert!(app.shown.contains(&repair));
        assert_eq!(texts(&app, repair), ["замер"]);

        // Пустой список удаляется совсем, после подтверждения.
        press(&mut app, KeyCode::Char('L'));
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "Пустой");
        press(&mut app, KeyCode::Enter);
        let empty = *app.registry.active().last().unwrap();
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));
        assert!(app.registry.get(empty).is_none());

        // Архивный — обратно на экран.
        let row = app.list_rows().iter().position(|&c| c == repair).unwrap();
        if let Mode::Lists { row: r, .. } = &mut app.mode {
            *r = row;
        }
        press(&mut app, KeyCode::Char('r'));
        assert!(app.registry.is_active(repair));
        // Всё сохранено в реестре.
        assert_eq!(app.book.lists().unwrap(), app.registry);
        cleanup(app);
    }

    #[test]
    fn grid_navigation() {
        let mut app = test_app("grid");
        // Шесть списков — сетка 3×2: 1 2 3 / 4 5 6.
        for title in ["Пятый", "Шестой"] {
            press(&mut app, KeyCode::Char('L'));
            press(&mut app, KeyCode::Char('a'));
            type_text(&mut app, title);
            press(&mut app, KeyCode::Enter);
            press(&mut app, KeyCode::Esc);
        }
        app.width = 100;
        let at = |app: &App| app.shown.iter().position(|&c| c == app.active).unwrap();
        press(&mut app, KeyCode::Char('l'));
        press(&mut app, KeyCode::Right);
        assert_eq!(at(&app), 2);
        press(&mut app, KeyCode::Char('l'));
        assert_eq!(at(&app), 2);
        press_with(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        assert_eq!(at(&app), 5);
        press(&mut app, KeyCode::Char('h'));
        press_with(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(at(&app), 1);
        press(&mut app, KeyCode::Char('6'));
        assert_eq!(at(&app), 5);
        press(&mut app, KeyCode::Tab);
        assert_eq!(at(&app), 0);
        cleanup(app);
    }

    #[test]
    fn privacy() {
        let mut app = test_app("privacy");
        press(&mut app, KeyCode::Char('3'));
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "личное дело");
        press(&mut app, KeyCode::Enter);
        // P — список скрыт, пометка в реестре списков.
        press(&mut app, KeyCode::Char('P'));
        assert!(app.is_hidden(ListId::PERSONAL));
        assert!(app.book.lists().unwrap().is_hidden(ListId::PERSONAL));

        // Из скрытого в открытый — задача уходит скрытой.
        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Char('1'));
        let task = &app.board.list(ListId::WORK)[0];
        assert!(task.private);
        // p — открыть; из открытого в скрытый — пометка не нужна, скрыт сам список.
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('p'));
        assert!(!app.board.list(ListId::WORK)[0].private);
        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Char('2'));
        assert!(!app.board.list(ListId::PERSONAL)[0].private);
        // Пометка сохраняется в файле и едет при переносе на дату.
        press(&mut app, KeyCode::Char('3'));
        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('2'));
        assert!(app.board.list(ListId::HOME)[0].private);
        press(&mut app, KeyCode::Char('>'));
        press(&mut app, KeyCode::Enter);
        assert!(app.book.load(d(27)).unwrap().list(ListId::HOME)[0].private);
        cleanup(app);
    }

    #[test]
    fn deleted_entries_are_archived() {
        let mut app = test_app("entries");
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "кран");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Char('s'));
        type_text(&mut app, "прокладка");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));
        let deleted = std::fs::read_to_string(app.book.dir().join(DELETED_NOTES)).unwrap();
        assert!(deleted.contains("— кран · удалённый пункт записей\n"), "{deleted}");
        assert!(deleted.contains("- [ ] прокладка\n"), "{deleted}");
        cleanup(app);
    }

    #[test]
    fn external_changes_are_picked_up() {
        let mut app = test_app("external");
        for text in ["первая", "вторая"] {
            press(&mut app, KeyCode::Char('a'));
            type_text(&mut app, text);
            press(&mut app, KeyCode::Enter);
        }
        assert_eq!(app.selected(), Some(1));
        assert!(!app.check_external());

        // Агент дописал задачу в начало — выделение остаётся на «вторая».
        let mut board = app.book.load(d(26)).unwrap();
        board.push(ListId::WORK, Task::new("от агента"));
        board.move_up(ListId::WORK, 2);
        board.move_up(ListId::WORK, 1);
        app.book.save(d(26), &board).unwrap();
        assert!(app.check_external());
        assert_eq!(texts(&app, ListId::WORK), ["от агента", "первая", "вторая"]);
        assert_eq!(app.selected(), Some(2));
        assert!(matches!(app.status, Some(Status::Info(_))));

        // Правка задачи, пока агент переставил задачи: правится та же задача.
        press(&mut app, KeyCode::Char('e'));
        let mut board = app.book.load(d(26)).unwrap();
        board.move_up(ListId::WORK, 2);
        app.book.save(d(26), &board).unwrap();
        type_text(&mut app, "!");
        press(&mut app, KeyCode::Enter);
        assert_eq!(texts(&app, ListId::WORK), ["от агента", "вторая!", "первая"]);

        // Записи: агент добавил заметку, пока пишем свою, — обе на месте.
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "моя");
        assert!(app.tick(Instant::now() + AUTOSAVE));
        let id = app.board.list(ListId::WORK)[1].id.clone().unwrap();
        let mut notes = app.book.load_notes(&id).unwrap();
        notes.add_note("26.10.2026 09:00", "заметка агента");
        app.book.save_notes(&id, &notes).unwrap();
        type_text(&mut app, " длиннее");
        save_note(&mut app);
        let file = app.book.load_notes(&id).unwrap().serialize();
        assert!(file.contains(" моя длиннее\n") && file.contains(" заметка агента\n"), "{file}");
        assert!(!file.contains(" моя\n"), "{file}");

        // Задачу дописали снаружи без id (руками на телефоне); у неё открыто окно записей,
        // а агент дал ей id и запись — окно на месте, запись видна.
        press(&mut app, KeyCode::Esc);
        let mut board = app.book.load(d(26)).unwrap();
        board.push(ListId::HOME, Task::new("снаружи"));
        app.book.save(d(26), &board).unwrap();
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('n'));
        assert!(app.board.list(ListId::HOME)[0].id.is_none());
        let mut board = app.book.load(d(26)).unwrap();
        board.get_mut(ListId::HOME, 0).unwrap().id = Some("agent1".into());
        app.book.save(d(26), &board).unwrap();
        let mut notes = crate::notes::Notes::default();
        notes.add_subtask("от агента");
        app.book.save_notes("agent1", &notes).unwrap();
        assert!(app.check_external());
        let view = app.notes.as_ref().expect("окно записей осталось");
        assert_eq!(view.id, "agent1");
        assert_eq!(view.notes.visible().len(), 1);
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('1'));
        press(&mut app, KeyCode::Char('g'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('n'));

        // Задачу удалили снаружи — окно записей закрылось.
        let mut board = app.book.load(d(26)).unwrap();
        board.remove(ListId::WORK, 1);
        app.book.save(d(26), &board).unwrap();
        press(&mut app, KeyCode::Char('j'));
        assert!(app.notes.is_none());
        assert_eq!(texts(&app, ListId::WORK), ["от агента", "первая"]);
        cleanup(app);
    }

    #[test]
    fn editor_round_trip() {
        let mut app = test_app("editor");
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "длинное");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('n'));
        press(&mut app, KeyCode::Char('E'));
        let path = app.editor_request.take().expect("запрос редактора");
        // «Редактор» записал файл.
        std::fs::write(&path, "Абзац из редактора\n- [ ] пункт\n").unwrap();
        app.after_editor(None);
        let task = &app.board.list(ListId::WORK)[0];
        let id = task.id.clone().expect("id после редактора");
        assert!(path.ends_with(format!("{id}.md")));
        assert_eq!(app.summaries[&id].subtasks, 1);
        assert_eq!(app.notes.as_ref().unwrap().notes.visible().len(), 2);
        // Редактор не запустился — ошибка в строке состояния, окно на месте.
        app.after_editor(Some("Редактор «nope» не запустился".into()));
        assert!(matches!(app.status, Some(Status::Error(_))));
        assert!(app.notes.is_some());
        cleanup(app);
    }

    #[test]
    fn text_fields_release_the_mouse() {
        let mut app = test_app("keyboard");
        assert!(!app.wants_text());
        press(&mut app, KeyCode::Char('a'));
        assert!(app.wants_text());
        type_text(&mut app, "x");
        press(&mut app, KeyCode::Enter);
        assert!(!app.wants_text());
        press(&mut app, KeyCode::Char('n'));
        assert!(!app.wants_text());
        press(&mut app, KeyCode::Char('a'));
        assert!(app.wants_text());
        type_text(&mut app, "заметка");
        press(&mut app, KeyCode::Esc);
        // Вопрос «закрыть?» — кнопки-подсказки снова нужны.
        assert!(!app.wants_text());
        cleanup(app);
    }

    #[test]
    fn help_and_about_close_on_any_key() {
        let mut app = test_app("help");
        press(&mut app, KeyCode::Char('?'));
        assert!(app.show_help);
        // Клавиша, закрывшая окно, ничего больше не делает.
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.show_help && !app.quit);
        press(&mut app, KeyCode::F(1));
        assert!(app.show_help);
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('o'));
        assert!(app.show_about);
        press(&mut app, KeyCode::Char('a'));
        assert!(!app.show_about && matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn navigation_stays_in_bounds() {
        let mut app = test_app("nav");
        for text in ["a", "b"] {
            app.board.push(ListId::WORK, Task::new(text));
        }
        app.select(ListId::WORK, Some(0));
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.selected(), Some(0));
        for _ in 0..5 {
            press(&mut app, KeyCode::Char('j'));
        }
        assert_eq!(app.selected(), Some(1));
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(app.selected(), Some(0));
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.selected(), Some(1));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.active, ListId::HOME);
        assert_eq!(app.selected(), None);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.selected(), None);
    }
}
