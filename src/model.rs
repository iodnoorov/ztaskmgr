use crate::clock::Date;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Work,
    Home,
    Personal,
    Other,
}

impl Category {
    pub const ALL: [Category; 4] = [
        Category::Work,
        Category::Home,
        Category::Personal,
        Category::Other,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Category::Work => "Работа",
            Category::Home => "Дом",
            Category::Personal => "Личное",
            Category::Other => "Прочее",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn from_index(index: usize) -> Option<Self> {
        Self::ALL.get(index).copied()
    }

    pub fn from_title(title: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.title() == title)
    }

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Open,
    Done,
    /// Перенесена на другую страницу; здесь остаётся как запись истории.
    Migrated(Date),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub text: String,
    pub state: State,
    /// Постоянный ключ задачи: по нему её узнают при слиянии копий с разных устройств,
    /// он же — имя файла доп. записей (`notes/<id>.md`). Переезжает вместе с задачей при
    /// переносе. Задача, написанная руками без id, получает его при первом сохранении.
    pub id: Option<String>,
    /// Скрыта от ИИ сама по себе (вдобавок к скрытым спискам); едет с задачей при переносе.
    pub private: bool,
}

impl Task {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            state: State::Open,
            id: None,
            private: false,
        }
    }

    /// Копия для страницы, на которую задачу переносят: снова открыта, записи те же.
    pub fn carried(&self) -> Self {
        Self {
            text: self.text.clone(),
            state: State::Open,
            id: self.id.clone(),
            private: self.private,
        }
    }

    pub fn is_open(&self) -> bool {
        self.state == State::Open
    }
}

/// Четыре списка задач. Позиция задачи в списке и есть её приоритет.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Board {
    lists: [Vec<Task>; 4],
}

impl Board {
    pub fn list(&self, category: Category) -> &[Task] {
        &self.lists[category.index()]
    }

    fn list_mut(&mut self, category: Category) -> &mut Vec<Task> {
        &mut self.lists[category.index()]
    }

    /// Добавляет задачу в конец списка и возвращает её индекс.
    pub fn push(&mut self, category: Category, task: Task) -> usize {
        let list = self.list_mut(category);
        list.push(task);
        list.len() - 1
    }

    pub fn remove(&mut self, category: Category, index: usize) -> Option<Task> {
        let list = self.list_mut(category);
        (index < list.len()).then(|| list.remove(index))
    }

    pub fn get_mut(&mut self, category: Category, index: usize) -> Option<&mut Task> {
        self.list_mut(category).get_mut(index)
    }

    /// Меняет задачу местами с соседней. Возвращает новый индекс, если сдвиг возможен.
    pub fn move_up(&mut self, category: Category, index: usize) -> Option<usize> {
        let list = self.list_mut(category);
        if index == 0 || index >= list.len() {
            return None;
        }
        list.swap(index, index - 1);
        Some(index - 1)
    }

    pub fn move_down(&mut self, category: Category, index: usize) -> Option<usize> {
        let list = self.list_mut(category);
        if index + 1 >= list.len() {
            return None;
        }
        list.swap(index, index + 1);
        Some(index + 1)
    }

    /// Отмечает задачу выполненной или снимает отметку. Выполненная уезжает в конец списка,
    /// снятая — в конец невыполненных (после последней открытой). Возвращает новый индекс.
    pub fn set_done(&mut self, category: Category, index: usize, done: bool) -> Option<usize> {
        let list = self.list_mut(category);
        let mut task = list.get(index)?.clone();
        if matches!(task.state, State::Migrated(_)) || (task.state == State::Done) == done {
            return Some(index);
        }
        list.remove(index);
        task.state = if done { State::Done } else { State::Open };
        let at = if done {
            list.len()
        } else {
            list.iter().rposition(Task::is_open).map_or(0, |i| i + 1)
        };
        list.insert(at, task);
        Some(at)
    }

    /// Вставляет задачу в список на место `index` (не дальше конца).
    pub fn insert(&mut self, category: Category, index: usize, task: Task) {
        let list = self.list_mut(category);
        list.insert(index.min(list.len()), task);
    }

    /// Даёт id задачам, у которых его нет. Возвращает, были ли такие.
    pub fn assign_ids(&mut self, mut new_id: impl FnMut() -> String) -> bool {
        let mut any = false;
        for task in self.lists.iter_mut().flatten().filter(|t| t.id.is_none()) {
            task.id = Some(new_id());
            any = true;
        }
        any
    }

    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.lists.iter().flatten()
    }

    pub fn is_empty(&self) -> bool {
        self.lists.iter().all(Vec::is_empty)
    }

    /// Помечает все невыполненные задачи перенесёнными на `to` и возвращает их копии
    /// (открытые, с категориями) для страницы `to`.
    pub fn migrate_open(&mut self, to: Date) -> Vec<(Category, Task)> {
        let mut moved = Vec::new();
        for category in Category::ALL {
            for task in self.list_mut(category).iter_mut().filter(|t| t.is_open()) {
                moved.push((category, task.carried()));
                task.state = State::Migrated(to);
            }
        }
        moved
    }

    /// Переносит задачу в конец другого списка. Возвращает её индекс там.
    pub fn move_to(&mut self, from: Category, index: usize, to: Category) -> Option<usize> {
        if from == to {
            return None;
        }
        let task = self.remove(from, index)?;
        Some(self.push(to, task))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board_with(category: Category, texts: &[&str]) -> Board {
        let mut board = Board::default();
        for text in texts {
            board.push(category, Task::new(*text));
        }
        board
    }

    fn texts(board: &Board, category: Category) -> Vec<&str> {
        board.list(category).iter().map(|t| t.text.as_str()).collect()
    }

    #[test]
    fn category_cycle() {
        assert_eq!(Category::Other.next(), Category::Work);
        assert_eq!(Category::Work.prev(), Category::Other);
        assert_eq!(Category::from_title("Дом"), Some(Category::Home));
        assert_eq!(Category::from_index(4), None);
    }

    #[test]
    fn move_within_list() {
        let mut board = board_with(Category::Work, &["a", "b", "c"]);
        assert_eq!(board.move_up(Category::Work, 0), None);
        assert_eq!(board.move_down(Category::Work, 2), None);
        assert_eq!(board.move_down(Category::Work, 0), Some(1));
        assert_eq!(texts(&board, Category::Work), ["b", "a", "c"]);
        assert_eq!(board.move_up(Category::Work, 2), Some(1));
        assert_eq!(texts(&board, Category::Work), ["b", "c", "a"]);
    }

    #[test]
    fn move_between_lists() {
        let mut board = board_with(Category::Work, &["a", "b"]);
        board.push(Category::Home, Task::new("x"));
        assert_eq!(board.move_to(Category::Work, 0, Category::Home), Some(1));
        assert_eq!(texts(&board, Category::Work), ["b"]);
        assert_eq!(texts(&board, Category::Home), ["x", "a"]);
        assert_eq!(board.move_to(Category::Work, 0, Category::Work), None);
        assert_eq!(board.move_to(Category::Work, 5, Category::Home), None);
    }

    #[test]
    fn done_sinks_down() {
        let order = |b: &Board| b.list(Category::Work).iter().map(|t| t.text.clone()).collect::<Vec<_>>();
        // Порядок: открытые, затем выполненные в порядке отметки.
        let mut board = board_with(Category::Work, &["a", "b", "c"]);
        assert_eq!(board.set_done(Category::Work, 0, true), Some(2));
        assert_eq!(board.set_done(Category::Work, 0, true), Some(2));
        assert_eq!(order(&board), ["c", "a", "b"]);
        // Уже выполненную повторно — на месте.
        assert_eq!(board.set_done(Category::Work, 1, true), Some(1));
        // Снятая отметка — в конец открытых.
        assert_eq!(board.set_done(Category::Work, 2, false), Some(1));
        assert_eq!(order(&board), ["c", "b", "a"]);
        assert_eq!(board.set_done(Category::Work, 9, true), None);
        // Перенесённую не трогаем.
        board.get_mut(Category::Work, 0).unwrap().state = State::Migrated(Date::new(2026, 10, 27).unwrap());
        assert_eq!(board.set_done(Category::Work, 0, true), Some(0));
    }

    #[test]
    fn migrate_open_only() {
        let mut board = board_with(Category::Work, &["a", "b"]);
        board.push(Category::Home, Task::new("c"));
        board.get_mut(Category::Work, 1).unwrap().state = State::Done;
        let to = Date::new(2026, 10, 27).unwrap();
        let moved = board.migrate_open(to);
        assert_eq!(
            moved,
            [(Category::Work, Task::new("a")), (Category::Home, Task::new("c"))]
        );
        assert_eq!(board.list(Category::Work)[0].state, State::Migrated(to));
        assert_eq!(board.list(Category::Work)[1].state, State::Done);
        assert!(board.migrate_open(to).is_empty());
        assert!(!board.is_empty());
        assert!(Board::default().is_empty());
    }

    #[test]
    fn remove_out_of_range() {
        let mut board = board_with(Category::Other, &["a"]);
        assert_eq!(board.remove(Category::Other, 1), None);
        assert_eq!(board.remove(Category::Other, 0), Some(Task::new("a")));
    }
}
