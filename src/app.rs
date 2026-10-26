use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use ratatui::widgets::ListState;

use crate::field::LineInput;
use crate::input::Press;
use crate::keymap::{self, Action};
use crate::model::{Board, Category, Task};
use crate::storage;

#[derive(Debug)]
pub enum Mode {
    Normal,
    /// Ввод текста: новая задача (`index == None`) или правка существующей.
    Edit { input: LineInput, index: Option<usize> },
    ConfirmDelete,
    /// Ожидание цифры категории после `m`.
    MoveTo,
}

pub struct App {
    pub board: Board,
    pub path: PathBuf,
    pub active: Category,
    /// Выделение и прокрутка — свои у каждого списка.
    pub lists: [ListState; 4],
    pub mode: Mode,
    pub show_help: bool,
    pub show_about: bool,
    /// Сообщение в строке состояния (ошибка сохранения и т. п.).
    pub status: Option<String>,
    pub quit: bool,
}

fn ctrl(press: &Press, c: char) -> bool {
    press.hotkey.code == KeyCode::Char(c) && press.hotkey.modifiers.contains(KeyModifiers::CONTROL)
}

impl App {
    pub fn new(board: Board, path: PathBuf) -> Self {
        let mut app = Self {
            board,
            path,
            active: Category::Work,
            lists: Default::default(),
            mode: Mode::Normal,
            show_help: false,
            show_about: false,
            status: None,
            quit: false,
        };
        for category in Category::ALL {
            app.clamp_selection(category);
        }
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

    fn save(&mut self) {
        self.status = storage::save(&self.path, &self.board)
            .err()
            .map(|e| format!("Ошибка сохранения: {e:#}"));
    }

    /// Нажатие: горячие клавиши — по физической клавише, поле ввода получает набранное.
    pub fn on_key(&mut self, press: impl Into<Press>) {
        let press = press.into();
        if ctrl(&press, 'c') {
            self.quit = true;
            return;
        }
        if self.show_help || self.show_about {
            self.show_help = false;
            self.show_about = false;
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
            Mode::ConfirmDelete => self.on_key_confirm(press),
            Mode::MoveTo => self.on_key_move_to(press),
        }
    }

    /// Вставленный текст идёт в открытое поле ввода как есть, иначе игнорируется:
    /// на главном экране он сработал бы как горячие клавиши.
    pub fn on_paste(&mut self, text: &str) {
        if let Mode::Edit { input, .. } = &mut self.mode {
            text.chars()
                .filter(|c| !c.is_control())
                .for_each(|c| input.insert(c));
        }
    }

    fn act(&mut self, action: Action) {
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
            Action::MoveTo if self.selected_task().is_some() => self.mode = Mode::MoveTo,
            Action::Delete if self.selected_task().is_some() => self.mode = Mode::ConfirmDelete,
            Action::MoveTo | Action::Delete => {}
        }
    }

    fn on_key_edit(&mut self, press: Press) {
        let Mode::Edit { input, index } = &mut self.mode else {
            return;
        };
        let typed = press.typed;
        match press.hotkey.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let text = input.text().trim().to_string();
                let index = *index;
                self.mode = Mode::Normal;
                self.commit_edit(index, text);
            }
            _ if ctrl(&press, 'w') => input.delete_word(),
            _ if ctrl(&press, 'a') => input.home(),
            _ if ctrl(&press, 'e') => input.end(),
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
    }

    fn on_key_confirm(&mut self, press: Press) {
        self.mode = Mode::Normal;
        if matches!(press.hotkey.code, KeyCode::Char('y') | KeyCode::Enter)
            && let Some(index) = self.selected()
        {
            self.board.remove(self.active, index);
            self.clamp_selection(self.active);
            self.save();
        }
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
        if let Some(task) = self.board.get_mut(self.active, index) {
            task.done = !task.done;
            self.save();
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

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

    fn test_app(name: &str) -> App {
        let path = std::env::temp_dir()
            .join(format!("ztaskmgr-app-{}-{name}", std::process::id()))
            .join("tasks.md");
        App::new(Board::default(), path)
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
        assert!(app.board.list(Category::Work)[0].done);

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

        let saved = storage::load(&app.path).unwrap();
        assert_eq!(saved, app.board);
        assert_eq!(app.status, None);
        std::fs::remove_dir_all(app.path.parent().unwrap()).unwrap();
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
        assert!(app.board.list(Category::Work)[0].done);
        press(&mut app, KeyCode::Char('в'));
        press(&mut app, KeyCode::Char('н'));
        assert!(app.board.list(Category::Work).is_empty());
        // Ctrl+W в поле ввода в русской раскладке.
        press(&mut app, KeyCode::Char('ф'));
        type_text(&mut app, "два слова");
        press_with(&mut app, KeyCode::Char('ц'), KeyModifiers::CONTROL);
        let Mode::Edit { input, .. } = &app.mode else { panic!() };
        assert_eq!(input.text(), "два ");
        std::fs::remove_dir_all(app.path.parent().unwrap()).unwrap();
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
        std::fs::remove_dir_all(app.path.parent().unwrap()).unwrap();
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
        let mut board = Board::default();
        for text in ["a", "b"] {
            board.push(Category::Work, Task::new(text));
        }
        let mut app = App::new(board, PathBuf::from("/nonexistent/tasks.md"));
        assert_eq!(app.selected(), Some(0));
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
