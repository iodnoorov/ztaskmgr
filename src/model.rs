use std::fmt;
use std::sync::{LazyLock, Mutex};

use crate::clock::Date;

/// Список задач — по постоянному id (`work`, `k3x9ab`). Название и место на экране хранит
/// реестр списков (`lists.rs`): их можно менять, не переписывая страниц. Id интернируются,
/// чтобы тип оставался `Copy` и сравнивался за число.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListId(u32);

/// Списки новой книжки; их id одинаковы на всех устройствах.
const DEFAULT_IDS: [&str; 4] = ["work", "home", "personal", "other"];

static IDS: LazyLock<Mutex<Vec<&'static str>>> = LazyLock::new(|| Mutex::new(DEFAULT_IDS.to_vec()));

impl ListId {
    pub const WORK: ListId = ListId(0);
    pub const HOME: ListId = ListId(1);
    pub const PERSONAL: ListId = ListId(2);
    pub const OTHER: ListId = ListId(3);
    pub const DEFAULTS: [ListId; 4] = [Self::WORK, Self::HOME, Self::PERSONAL, Self::OTHER];

    pub fn new(id: &str) -> Self {
        let mut ids = IDS.lock().unwrap_or_else(|e| e.into_inner());
        let n = match ids.iter().position(|&known| known == id) {
            Some(n) => n,
            None => {
                ids.push(Box::leak(id.to_string().into_boxed_str()));
                ids.len() - 1
            }
        };
        ListId(n as u32)
    }

    pub fn as_str(self) -> &'static str {
        IDS.lock().unwrap_or_else(|e| e.into_inner())[self.0 as usize]
    }
}

impl fmt::Debug for ListId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ListId({})", self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Open,
    Done,
    /// Отменена: не сделана и делать не нужно. Не переносится, в счётчике не участвует.
    Cancelled,
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

/// Страница: списки задач по id, в порядке, в каком они в файле. Позиция задачи в списке
/// и есть её приоритет. Списка без задач может не быть вовсе.
#[derive(Clone, Debug, Default)]
pub struct Board {
    lists: Vec<(ListId, Vec<Task>)>,
    /// Названия списков из заголовков файла — на случай, если списка нет в реестре
    /// (страница пришла с другого устройства раньше реестра).
    titles: Vec<(ListId, String)>,
}

/// Страницы равны, если равны их списки; порядок списков и названия из файла не важны.
impl PartialEq for Board {
    fn eq(&self, other: &Self) -> bool {
        self.lists.iter().chain(&other.lists).all(|(id, _)| self.list(*id) == other.list(*id))
    }
}

impl Eq for Board {}

impl Board {
    pub fn list(&self, category: ListId) -> &[Task] {
        self.lists.iter().find(|(id, _)| *id == category).map_or(&[], |(_, list)| list)
    }

    fn list_mut(&mut self, category: ListId) -> &mut Vec<Task> {
        let n = match self.lists.iter().position(|(id, _)| *id == category) {
            Some(n) => n,
            None => {
                self.lists.push((category, Vec::new()));
                self.lists.len() - 1
            }
        };
        &mut self.lists[n].1
    }

    /// Списки, в которых есть задачи, в порядке файла.
    pub fn ids(&self) -> Vec<ListId> {
        self.lists.iter().filter(|(_, l)| !l.is_empty()).map(|(id, _)| *id).collect()
    }

    /// Название списка из заголовка файла.
    pub fn title(&self, category: ListId) -> Option<&str> {
        self.titles.iter().find(|(id, _)| *id == category).map(|(_, t)| t.as_str())
    }

    pub fn set_title(&mut self, category: ListId, title: &str) {
        self.titles.retain(|(id, _)| *id != category);
        self.titles.push((category, title.to_string()));
    }

    /// Добавляет задачу в конец списка и возвращает её индекс.
    pub fn push(&mut self, category: ListId, task: Task) -> usize {
        let list = self.list_mut(category);
        list.push(task);
        list.len() - 1
    }

    pub fn remove(&mut self, category: ListId, index: usize) -> Option<Task> {
        let list = self.list_mut(category);
        (index < list.len()).then(|| list.remove(index))
    }

    pub fn get_mut(&mut self, category: ListId, index: usize) -> Option<&mut Task> {
        self.list_mut(category).get_mut(index)
    }

    /// Меняет задачу местами с соседней. Возвращает новый индекс, если сдвиг возможен.
    pub fn move_up(&mut self, category: ListId, index: usize) -> Option<usize> {
        let list = self.list_mut(category);
        if index == 0 || index >= list.len() {
            return None;
        }
        list.swap(index, index - 1);
        Some(index - 1)
    }

    pub fn move_down(&mut self, category: ListId, index: usize) -> Option<usize> {
        let list = self.list_mut(category);
        if index + 1 >= list.len() {
            return None;
        }
        list.swap(index, index + 1);
        Some(index + 1)
    }

    /// Отмечает задачу выполненной или отменённой или снимает отметку. Закрытая уезжает в конец
    /// списка, снятая — в конец невыполненных (после последней открытой). Возвращает новый
    /// индекс. Перенесённую не трогает, `state` не может быть `Migrated`.
    pub fn set_state(&mut self, category: ListId, index: usize, state: State) -> Option<usize> {
        let list = self.list_mut(category);
        let mut task = list.get(index)?.clone();
        if matches!(task.state, State::Migrated(_)) || matches!(state, State::Migrated(_)) || task.state == state {
            return Some(index);
        }
        list.remove(index);
        task.state = state;
        let at = if state == State::Open {
            list.iter().rposition(Task::is_open).map_or(0, |i| i + 1)
        } else {
            list.len()
        };
        list.insert(at, task);
        Some(at)
    }

    /// Вставляет задачу в список на место `index` (не дальше конца).
    pub fn insert(&mut self, category: ListId, index: usize, task: Task) {
        let list = self.list_mut(category);
        list.insert(index.min(list.len()), task);
    }

    /// Даёт id задачам, у которых его нет. Возвращает, были ли такие.
    pub fn assign_ids(&mut self, mut new_id: impl FnMut() -> String) -> bool {
        let mut any = false;
        for task in self.lists.iter_mut().flat_map(|(_, l)| l).filter(|t| t.id.is_none()) {
            task.id = Some(new_id());
            any = true;
        }
        any
    }

    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.lists.iter().flat_map(|(_, l)| l)
    }

    pub fn is_empty(&self) -> bool {
        self.lists.iter().all(|(_, l)| l.is_empty())
    }

    /// Помечает все невыполненные задачи перенесёнными на `to` и возвращает их копии
    /// (открытые, со списками) для страницы `to`.
    pub fn migrate_open(&mut self, to: Date) -> Vec<(ListId, Task)> {
        let mut moved = Vec::new();
        for (category, list) in &mut self.lists {
            for task in list.iter_mut().filter(|t| t.is_open()) {
                moved.push((*category, task.carried()));
                task.state = State::Migrated(to);
            }
        }
        moved
    }

    /// Переносит задачу в конец другого списка. Возвращает её индекс там.
    pub fn move_to(&mut self, from: ListId, index: usize, to: ListId) -> Option<usize> {
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

    fn board_with(category: ListId, texts: &[&str]) -> Board {
        let mut board = Board::default();
        for text in texts {
            board.push(category, Task::new(*text));
        }
        board
    }

    fn texts(board: &Board, category: ListId) -> Vec<&str> {
        board.list(category).iter().map(|t| t.text.as_str()).collect()
    }

    #[test]
    fn list_ids() {
        assert_eq!(ListId::new("home"), ListId::HOME);
        let new = ListId::new("k3x9ab");
        assert_eq!(new, ListId::new("k3x9ab"));
        assert_eq!(new.as_str(), "k3x9ab");
        // Равенство страниц — по задачам, без пустых списков и порядка.
        let mut a = Board::default();
        a.push(ListId::WORK, Task::new("x"));
        a.push(new, Task::new("y"));
        let mut b = Board::default();
        b.push(new, Task::new("y"));
        b.list_mut(ListId::HOME);
        b.push(ListId::WORK, Task::new("x"));
        assert_eq!(a, b);
        assert_eq!(b.ids(), [new, ListId::WORK]);
    }

    #[test]
    fn move_within_list() {
        let mut board = board_with(ListId::WORK, &["a", "b", "c"]);
        assert_eq!(board.move_up(ListId::WORK, 0), None);
        assert_eq!(board.move_down(ListId::WORK, 2), None);
        assert_eq!(board.move_down(ListId::WORK, 0), Some(1));
        assert_eq!(texts(&board, ListId::WORK), ["b", "a", "c"]);
        assert_eq!(board.move_up(ListId::WORK, 2), Some(1));
        assert_eq!(texts(&board, ListId::WORK), ["b", "c", "a"]);
    }

    #[test]
    fn move_between_lists() {
        let mut board = board_with(ListId::WORK, &["a", "b"]);
        board.push(ListId::HOME, Task::new("x"));
        assert_eq!(board.move_to(ListId::WORK, 0, ListId::HOME), Some(1));
        assert_eq!(texts(&board, ListId::WORK), ["b"]);
        assert_eq!(texts(&board, ListId::HOME), ["x", "a"]);
        assert_eq!(board.move_to(ListId::WORK, 0, ListId::WORK), None);
        assert_eq!(board.move_to(ListId::WORK, 5, ListId::HOME), None);
    }

    #[test]
    fn done_sinks_down() {
        let order = |b: &Board| b.list(ListId::WORK).iter().map(|t| t.text.clone()).collect::<Vec<_>>();
        let (open, done, cancelled) = (State::Open, State::Done, State::Cancelled);
        // Порядок: открытые, затем закрытые в порядке отметки.
        let mut board = board_with(ListId::WORK, &["a", "b", "c"]);
        assert_eq!(board.set_state(ListId::WORK, 0, done), Some(2));
        assert_eq!(board.set_state(ListId::WORK, 0, cancelled), Some(2));
        assert_eq!(order(&board), ["c", "a", "b"]);
        // Уже выполненную повторно — на месте.
        assert_eq!(board.set_state(ListId::WORK, 1, done), Some(1));
        // Снятая отметка — в конец открытых.
        assert_eq!(board.set_state(ListId::WORK, 2, open), Some(1));
        assert_eq!(order(&board), ["c", "b", "a"]);
        // Отменённая → выполненная: снова в конец.
        board.set_state(ListId::WORK, 0, cancelled);
        assert_eq!(order(&board), ["b", "a", "c"]);
        assert_eq!(board.set_state(ListId::WORK, 2, done), Some(2));
        assert_eq!(board.list(ListId::WORK)[2].state, done);
        assert_eq!(board.set_state(ListId::WORK, 9, done), None);
        // Перенесённую не трогаем, перенести так нельзя.
        let to = State::Migrated(Date::new(2026, 10, 27).unwrap());
        assert_eq!(board.set_state(ListId::WORK, 0, to), Some(0));
        assert_eq!(board.list(ListId::WORK)[0].state, open);
        board.get_mut(ListId::WORK, 0).unwrap().state = to;
        assert_eq!(board.set_state(ListId::WORK, 0, done), Some(0));
    }

    #[test]
    fn migrate_open_only() {
        let mut board = board_with(ListId::WORK, &["a", "b"]);
        board.push(ListId::HOME, Task::new("c"));
        board.get_mut(ListId::WORK, 1).unwrap().state = State::Done;
        board.push(ListId::WORK, Task::new("d"));
        board.get_mut(ListId::WORK, 2).unwrap().state = State::Cancelled;
        let to = Date::new(2026, 10, 27).unwrap();
        let moved = board.migrate_open(to);
        assert_eq!(
            moved,
            [(ListId::WORK, Task::new("a")), (ListId::HOME, Task::new("c"))]
        );
        assert_eq!(board.list(ListId::WORK)[0].state, State::Migrated(to));
        assert_eq!(board.list(ListId::WORK)[1].state, State::Done);
        assert_eq!(board.list(ListId::WORK)[2].state, State::Cancelled);
        assert!(board.migrate_open(to).is_empty());
        assert!(!board.is_empty());
        assert!(Board::default().is_empty());
    }

    #[test]
    fn remove_out_of_range() {
        let mut board = board_with(ListId::OTHER, &["a"]);
        assert_eq!(board.remove(ListId::OTHER, 1), None);
        assert_eq!(board.remove(ListId::OTHER, 0), Some(Task::new("a")));
    }
}
