use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use ratatui::widgets::ListState;

use crate::book::{Book, DELETED_NOTES};
use crate::clock::{self, Date};
use crate::field::{LineInput, TextArea};
use crate::input::Press;
use crate::keymap::{self, Action};
use crate::model::{Board, Category, State, Task};
use crate::notes::{Notes, Summary};

#[derive(Debug)]
pub enum Mode {
    Normal,
    /// Ввод текста: новая задача (`index == None`) или правка существующей.
    Edit { input: LineInput, index: Option<usize> },
    /// Ввод даты: перейти к странице или перенести задачу.
    Date { input: LineInput, purpose: DatePurpose },
    ConfirmDelete,
    /// Ожидание цифры категории после `m`.
    MoveTo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatePurpose {
    Go,
    /// Перенос задачи с этим индексом в активном списке.
    Postpone(usize),
}

/// Окно доп. записей задачи, поверх страницы.
pub struct NotesView {
    pub category: Category,
    /// Индекс задачи в списке; пока окно открыто, страница не меняется.
    pub index: usize,
    /// id записей; у задачи без записей — новый, задаче он достаётся с первой записью.
    pub id: String,
    pub notes: Notes,
    /// Выделение — среди пунктов, `Notes::visible`.
    pub list: ListState,
    pub mode: NotesMode,
}

pub enum NotesMode {
    List,
    /// Однострочное поле — подзадача.
    Edit { input: LineInput, target: NoteTarget },
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
    pub active: Category,
    /// Выделение и прокрутка — свои у каждого списка.
    pub lists: [ListState; 4],
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
    pub fn new(book: Book, today: Date) -> Self {
        let mut app = Self {
            book,
            date: today,
            today,
            board: Board::default(),
            page_error: None,
            active: Category::Work,
            lists: Default::default(),
            mode: Mode::Normal,
            show_help: false,
            show_about: false,
            status: None,
            notes: None,
            summaries: HashMap::new(),
            editor_request: None,
            quit: false,
        };
        app.open_page(today);
        app
    }

    pub fn selected(&self) -> Option<usize> {
        self.lists[self.active.index()].selected()
    }

    pub fn selected_task(&self) -> Option<&Task> {
        self.board.list(self.active).get(self.selected()?)
    }

    fn select(&mut self, category: Category, index: Option<usize>) {
        self.lists[category.index()].select(index);
        self.clamp_selection(category);
    }

    /// Держит выделение в границах списка; у непустого списка что-то выделено всегда.
    fn clamp_selection(&mut self, category: Category) {
        let len = self.board.list(category).len();
        let state = &mut self.lists[category.index()];
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
        self.lists = Default::default();
        for category in Category::ALL {
            self.clamp_selection(category);
        }
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
        match self.book.save(self.date, &self.board) {
            Ok(()) => {
                if matches!(self.status, Some(Status::Error(_))) {
                    self.status = None;
                }
            }
            Err(e) => self.status = Some(Status::Error(format!("Ошибка сохранения: {e:#}"))),
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
            Mode::MoveTo => self.on_key_move_to(press),
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
                | Action::MoveUp
                | Action::MoveDown
                | Action::MoveTo
                | Action::Postpone
                | Action::Delete
                | Action::Notes
        );
        if edits && !self.editable() {
            return;
        }
        let list = &mut self.lists[self.active.index()];
        match action {
            Action::Quit => self.quit = true,
            Action::Help => self.show_help = true,
            Action::About => self.show_about = true,
            Action::List(i) => self.active = Category::from_index(i).unwrap_or(self.active),
            Action::NextList => self.active = self.active.next(),
            Action::PrevList => self.active = self.active.prev(),
            // select_previous/next у пустого списка выставляют индекс за границей — поправляем.
            Action::Up => {
                list.select_previous();
                self.clamp_selection(self.active);
            }
            Action::Down => {
                list.select_next();
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
            Action::MoveUp => self.move_task(Board::move_up),
            Action::MoveDown => self.move_task(Board::move_down),
            Action::Postpone => self.start_postpone(),
            Action::MoveTo if self.selected_task().is_some() => self.mode = Mode::MoveTo,
            Action::Delete if self.selected_task().is_some() => self.mode = Mode::ConfirmDelete,
            Action::Notes => self.open_notes(),
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

    fn on_key_move_to(&mut self, press: Press) {
        self.mode = Mode::Normal;
        let KeyCode::Char(c @ '1'..='4') = press.hotkey.code else {
            return;
        };
        let to = Category::from_index(c as usize - '1' as usize).unwrap();
        let Some(index) = self.selected() else {
            return;
        };
        if let Some(new_index) = self.board.move_to(self.active, index, to) {
            self.clamp_selection(self.active);
            self.select(to, Some(new_index));
            self.save();
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
        let Some(task) = self.board.get_mut(self.active, index) else {
            return;
        };
        task.state = match task.state {
            State::Open => State::Done,
            State::Done => State::Open,
            State::Migrated(to) => {
                let when = to.format(c"%-d %B");
                self.status = Some(Status::Info(format!(
                    "Задача перенесена на {when} — отметьте её там"
                )));
                return;
            }
        };
        self.save();
    }

    /// Сдвиг внутри списка (`Board::move_up` / `move_down`).
    fn move_task(&mut self, shift: fn(&mut Board, Category, usize) -> Option<usize>) {
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
        match self.book.load_notes(&id) {
            Ok(notes) => {
                let mut view = NotesView {
                    category: self.active,
                    index,
                    id,
                    notes,
                    list: ListState::default(),
                    mode: NotesMode::List,
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
            NotesMode::Edit { input, target } => match edit_line(input, &press) {
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
                    view.notes.remove(entry);
                    view.clamp();
                    self.save_notes();
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
                view.notes.remove(entry);
                view.clamp();
                self.save_notes();
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

    /// Закрывает многострочное поле; сохранённое остаётся как есть.
    fn close_draft(&mut self) {
        if let Some(view) = &mut self.notes {
            view.mode = NotesMode::List;
        }
    }

    /// Ход часов: автосохранение заметки. Возвращает, сохранилось ли (перерисовать).
    pub fn tick(&mut self, now: Instant) -> bool {
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
        due
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
                view.notes.remove(entry);
                view.clamp();
                self.save_notes();
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
                            input: LineInput::new(text),
                            target,
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
        App::new(Book::new(dir), d(26))
    }

    fn cleanup(app: App) {
        let _ = std::fs::remove_dir_all(app.book.dir());
    }

    fn texts(app: &App, category: Category) -> Vec<String> {
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
        assert_eq!(texts(&app, Category::Work), ["первая", "вторая"]);
        assert_eq!(app.selected(), Some(1));

        press_with(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(texts(&app, Category::Work), ["вторая", "первая"]);
        assert_eq!(app.selected(), Some(0));
        // J/K больше не сдвигают.
        press(&mut app, KeyCode::Char('J'));
        assert_eq!(texts(&app, Category::Work), ["вторая", "первая"]);

        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.board.list(Category::Work)[0].state, State::Done);

        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Char('3'));
        assert_eq!(texts(&app, Category::Work), ["первая"]);
        assert_eq!(texts(&app, Category::Personal), ["вторая"]);
        assert_eq!(app.lists[Category::Personal.index()].selected(), Some(0));

        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(texts(&app, Category::Work), ["первая"]);
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));
        assert!(app.board.list(Category::Work).is_empty());
        assert_eq!(app.selected(), None);

        assert_eq!(app.book.load(d(26)).unwrap(), app.board);
        assert_eq!(app.status, None);
        cleanup(app);
    }

    #[test]
    fn russian_layout() {
        let mut app = test_app("ru");
        // «ф» — физическая a: новая задача; в поле ввода — сама «ф».
        press(&mut app, KeyCode::Char('ф'));
        type_text(&mut app, "фото");
        press(&mut app, KeyCode::Enter);
        assert_eq!(texts(&app, Category::Work), ["фото"]);
        // «ч» — x: выполнено; «в» + «н» — d + y: удалить.
        press(&mut app, KeyCode::Char('ч'));
        assert_eq!(app.board.list(Category::Work)[0].state, State::Done);
        press(&mut app, KeyCode::Char('в'));
        press(&mut app, KeyCode::Char('н'));
        assert!(app.board.list(Category::Work).is_empty());
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
        assert_eq!(texts(&app, Category::Home), ["кран"]);

        press(&mut app, KeyCode::Char('e'));
        press(&mut app, KeyCode::Home);
        app.on_paste("Починить\n ");
        press(&mut app, KeyCode::Enter);
        assert_eq!(texts(&app, Category::Home), ["Починить кран"]);

        // Пустой ввод не создаёт задачу.
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "   ");
        press(&mut app, KeyCode::Enter);
        assert_eq!(texts(&app, Category::Home).len(), 1);
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
        assert_eq!(app.board.list(Category::Work)[0].state, State::Migrated(d(27)));
        assert!(matches!(app.status, Some(Status::Info(_))));
        // Перенесённую не отметить и не перенести второй раз.
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.board.list(Category::Work)[0].state, State::Migrated(d(27)));
        press(&mut app, KeyCode::Char('>'));
        assert!(matches!(app.mode, Mode::Normal));

        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.date, d(27));
        assert_eq!(texts(&app, Category::Work), ["позвонить"]);
        assert!(app.board.list(Category::Work)[0].is_open());

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
        assert_eq!(texts(&app, Category::Work), ["недоделано"]);
        assert!(matches!(app.status, Some(Status::Info(_))));
        press(&mut app, KeyCode::Char('['));
        assert_eq!(app.board.list(Category::Work)[0].state, State::Migrated(d(27)));
        cleanup(app);
    }

    #[test]
    fn notes_window() {
        let mut app = test_app("notes");
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, "кран");
        press(&mut app, KeyCode::Enter);

        // Открыли и закрыли без записей — id у задачи не появился.
        press(&mut app, KeyCode::Char('n'));
        assert!(app.notes.is_some());
        press(&mut app, KeyCode::Esc);
        assert!(app.notes.is_none());
        assert_eq!(app.board.list(Category::Home)[0].id, None);

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

        let id = app.board.list(Category::Home)[0].id.clone().expect("id с первой записью");
        assert_eq!(app.board.list(Category::Home)[0].state, State::Open);
        let notes = app.book.load_notes(&id).unwrap().serialize();
        let lines: Vec<&str> = notes.lines().collect();
        assert_eq!(lines[0], "- [x] купить прокладку");
        assert!(lines[1].starts_with("- 2026-") && lines[1].ends_with(" мастер сказал"), "{notes}");
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
        let id = app.board.list(Category::Work)[0].id.clone().unwrap();

        // Перенос — копия с теми же записями.
        press(&mut app, KeyCode::Char('>'));
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char(']'));
        assert_eq!(app.board.list(Category::Work)[0].id.as_deref(), Some(id.as_str()));
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
        let id = app.board.list(Category::Work)[0].id.clone().unwrap();
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
        let id = app.board.list(Category::Work)[0].id.clone()?;
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
        let task = &app.board.list(Category::Work)[0];
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
            app.board.push(Category::Work, Task::new(text));
        }
        app.select(Category::Work, Some(0));
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
        assert_eq!(app.active, Category::Home);
        assert_eq!(app.selected(), None);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.selected(), None);
    }
}
