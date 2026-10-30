//! Окно записей задачи: подзадачи, заметки, черновик с автосохранением.

use super::*;

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
    pub(super) stamp: Option<Stamp>,
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

/// Как часто сохранять заметку, пока её пишут.
pub const AUTOSAVE: Duration = Duration::from_secs(10);

/// Многострочная заметка в работе. Сохраняется сама раз в `AUTOSAVE`; Esc закрывает поле,
/// не трогая уже сохранённое, а при несохранённых изменениях сначала спрашивает.
pub struct Draft {
    pub area: TextArea,
    /// `Note` — новая заметка до первого сохранения, потом (и при правке) — `Entry`.
    pub target: NoteTarget,
    /// Метка новой заметки — когда её начали писать.
    pub(super) stamp: String,
    /// Текст на момент последнего сохранения.
    pub(super) saved: String,
    pub(super) saved_at: Instant,
    /// Когда сохранилось в последний раз (`20:46:10`) — для показа.
    pub saved_label: Option<String>,
    /// Esc при несохранённых изменениях: окно вопроса.
    pub confirm_close: bool,
}

/// Пустые строки по краям заметки не нужны, внутри — остаются.
pub(super) fn normalize(text: &str) -> String {
    text.trim_end().trim_start_matches('\n').to_string()
}

impl Draft {
    pub(super) fn new(text: String, target: NoteTarget) -> Self {
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
pub(super) enum Done {
    Save,
    Cancel,
}

/// Правка многострочного поля. Возвращает, если поле пора закрыть.
pub(super) fn edit_area(area: &mut TextArea, press: &Press) -> Option<Done> {
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
                && !typed.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
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

    pub(super) fn clamp(&mut self) {
        let len = self.notes.visible().len();
        self.list.select(match (len, self.list.selected()) {
            (0, _) => None,
            (_, None) => Some(0),
            (_, Some(i)) => Some(i.min(len - 1)),
        });
    }
}

impl App {
    pub(super) fn open_notes(&mut self) {
        let Some(index) = self.selected() else {
            return;
        };
        let task = &self.board.list(self.active)[index];
        let id = match task.id.clone().map(Ok).unwrap_or_else(|| self.book.new_id()) {
            Ok(id) => id,
            Err(e) => {
                self.status = Some(Status::Error(format!("Записи задачи: {e:#}")));
                return;
            }
        };
        match self.book.open_notes(&id) {
            Ok((notes, stamp)) => {
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

    pub(super) fn on_key_notes(&mut self, press: Press) {
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
    pub(super) fn save_draft(&mut self, explicit: bool) {
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
    pub(super) fn remove_entry(&mut self, entry: usize) {
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
    pub(super) fn close_draft(&mut self) {
        if let Some(view) = &mut self.notes {
            view.mode = NotesMode::List;
        }
    }

    /// Ход часов: автосохранение заметки. Возвращает, было ли что (перерисовать).
    pub fn tick(&mut self, now: Instant) -> bool {
        // Сначала — то, что успели записать снаружи, чтобы автосохранение его не затёрло.
        let changed = self.check_external();
        let due = match &self.notes {
            Some(NotesView { mode: NotesMode::Text(draft), .. }) => {
                draft.is_changed() && !draft.is_empty() && now.duration_since(draft.saved_at) >= AUTOSAVE
            }
            _ => false,
        };
        if due {
            self.save_draft(false);
        }
        due || changed
    }

    /// Закрывает однострочное поле окна записей и сохраняет введённое. Пустое у нового
    /// пункта — отмена, у существующего — удаление пункта.
    pub(super) fn commit_note(&mut self, target: NoteTarget, text: &str) {
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

    pub(super) fn act_notes(&mut self, action: Action) {
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
                        NotesMode::Edit { input: LineInput::new(text.clone()), target, original: text }
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
    pub(super) fn save_notes(&mut self) {
        let Some(view) = &mut self.notes else {
            return;
        };
        let (id, category, index) = (view.id.clone(), view.category, view.index);
        self.edited = true;
        if let Err(e) = self.book.save_notes_draft(&id, &mut view.notes, &mut view.stamp) {
            self.status = Some(Status::Error(format!("Записи задачи: {e:#}")));
            return;
        }
        let summary = view.notes.summary();
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
        self.edited = true;
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

    pub(super) fn reload_notes(&mut self) {
        let Some(view) = &mut self.notes else {
            return;
        };
        let (notes, stamp) = match self.book.open_notes(&view.id) {
            Ok(snapshot) => snapshot,
            Err(e) => {
                self.status = Some(Status::Error(format!("Записи задачи: {e:#}")));
                return;
            }
        };
        view.stamp = stamp;
        // Пункт, который правится, — по его тексту до правки (или последнему сохранённому).
        let find = |text: &str| notes.visible().into_iter().find(|&i| notes.editable_text(i) == text);
        match &mut view.mode {
            NotesMode::Text(draft) => {
                if let NoteTarget::Entry(_) = draft.target {
                    // Пропал — при сохранении добавится заново, ничего не потеряется.
                    draft.target = find(&draft.saved).map_or(NoteTarget::Note, NoteTarget::Entry);
                }
            }
            NotesMode::Edit { target: target @ NoteTarget::Entry(_), original, .. } => match find(original) {
                Some(i) => *target = NoteTarget::Entry(i),
                None => view.mode = NotesMode::List,
            },
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

    /// Записи удалённой задачи — в `DELETED_NOTES.md`, если её копий больше нигде нет.
    pub(super) fn archive_notes(&mut self, task: &Task) {
        match self.book.archive_notes(task, self.date, &clock::stamp()) {
            Ok(true) => {
                self.status = Some(Status::Info(format!("Записи задачи сохранены в {DELETED_NOTES}")));
            }
            Ok(false) => {}
            Err(e) => self.status = Some(Status::Error(format!("Записи задачи: {e:#}"))),
        }
        self.summaries = self.book.summaries(&self.board);
    }
}
