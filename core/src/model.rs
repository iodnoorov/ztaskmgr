use std::collections::HashSet;
use std::fmt;
use std::sync::{LazyLock, Mutex};

use crate::clock::{self, Date};

/// Список задач — по постоянному id (`work`, `k3x9ab`). Название и место на экране хранит
/// реестр списков (`lists.rs`): их можно менять, не переписывая страниц. Внутри — ссылка
/// на строку с `'static`: тип `Copy`, `as_str` бесплатный, сравнение — по содержимому.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListId(&'static str);

/// Id, встреченные за время работы: каждая строка хранится один раз, а не при каждом чтении
/// страницы заново. Нужно только при создании id, а не при его использовании.
static IDS: LazyLock<Mutex<HashSet<&'static str>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

impl ListId {
    /// Списки новой книжки; их id одинаковы на всех устройствах.
    pub const WORK: ListId = ListId("work");
    pub const HOME: ListId = ListId("home");
    pub const PERSONAL: ListId = ListId("personal");
    pub const OTHER: ListId = ListId("other");
    pub const DEFAULTS: [ListId; 4] = [Self::WORK, Self::HOME, Self::PERSONAL, Self::OTHER];

    pub fn new(id: &str) -> Self {
        if let Some(known) = Self::DEFAULTS.into_iter().find(|d| d.0 == id) {
            return known;
        }
        let mut ids = IDS.lock().unwrap_or_else(|e| e.into_inner());
        match ids.get(id) {
            Some(&known) => ListId(known),
            None => {
                let leaked: &'static str = Box::leak(id.to_string().into_boxed_str());
                ids.insert(leaked);
                ListId(leaked)
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        self.0
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
    /// Когда задачу меняли в последний раз (мс с 1970-го; 0 — неизвестно, файл старой версии).
    /// При слиянии двух версий страницы побеждает более поздняя правка.
    pub modified: u64,
    /// Служебные пометки `ключ:значение`, которых эта версия не знает (от более новой):
    /// сохраняются как есть, чтобы правка старой программой их не потеряла.
    pub extra: Vec<String>,
}

impl Task {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into(), state: State::Open, id: None, private: false, modified: 0, extra: Vec::new() }
    }

    /// Копия для страницы, на которую задачу переносят: снова открыта, записи те же.
    pub fn carried(&self) -> Self {
        Self {
            text: self.text.clone(),
            state: State::Open,
            id: self.id.clone(),
            private: self.private,
            modified: clock::now_ms(),
            extra: self.extra.clone(),
        }
    }

    /// Отметить, что задачу только что поменяли.
    pub fn touch(&mut self) {
        self.modified = clock::now_ms();
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
    /// Надгробия: id удалённых задач и когда их удалили. По ним слияние не воскрешает
    /// удалённое, если после удаления задачу нигде не меняли.
    deleted: Vec<(String, u64)>,
    /// Версия снимка для условного сохранения; не часть формата или равенства досок.
    pub(crate) revision: std::cell::Cell<Option<(crate::clock::Date, Option<crate::store::Stamp>)>>,
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

    /// Убирает задачу из списка (например, чтобы переложить в другой) — без надгробия.
    pub fn remove(&mut self, category: ListId, index: usize) -> Option<Task> {
        let list = self.list_mut(category);
        (index < list.len()).then(|| list.remove(index))
    }

    /// Удаляет задачу совсем: с надгробием, чтобы при слиянии с другой версией страницы она
    /// не вернулась.
    pub fn delete(&mut self, category: ListId, index: usize) -> Option<Task> {
        let task = self.remove(category, index)?;
        if let Some(id) = &task.id {
            self.bury(id, clock::now_ms());
        }
        Some(task)
    }

    /// Надгробие (позднейшее из двух, если оно уже есть).
    pub fn bury(&mut self, id: &str, when: u64) {
        match self.deleted.iter_mut().find(|(i, _)| i == id) {
            Some((_, t)) => *t = (*t).max(when),
            None => self.deleted.push((id.to_string(), when)),
        }
    }

    /// Когда удалили задачу с этим id (если удаляли).
    pub fn buried(&self, id: &str) -> Option<u64> {
        self.deleted.iter().find(|(i, _)| i == id).map(|(_, t)| *t)
    }

    pub fn graves(&self) -> &[(String, u64)] {
        &self.deleted
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
        task.touch();
        let at =
            if state == State::Open { list.iter().rposition(Task::is_open).map_or(0, |i| i + 1) } else { list.len() };
        list.insert(at, task);
        Some(at)
    }

    /// Вставляет задачу в список на место `index` (не дальше конца).
    pub fn insert(&mut self, category: ListId, index: usize, task: Task) {
        let list = self.list_mut(category);
        list.insert(index.min(list.len()), task);
    }

    pub fn tasks(&self) -> impl Iterator<Item = &Task> {
        self.lists.iter().flat_map(|(_, l)| l)
    }
    /// Один id обозначает одну задачу на странице; исторические копии на других датах допустимы.
    pub fn validate_ids(&self) -> anyhow::Result<()> {
        let mut ids = std::collections::HashSet::new();
        for id in self.tasks().filter_map(|task| task.id.as_deref()) {
            anyhow::ensure!(ids.insert(id), "повторяющийся id задачи «{id}» на одной странице");
        }
        Ok(())
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
                task.touch();
            }
        }
        moved
    }

    /// Переносит задачу в конец другого списка. Возвращает её индекс там.
    pub fn move_to(&mut self, from: ListId, index: usize, to: ListId) -> Option<usize> {
        if from == to {
            return None;
        }
        let mut task = self.remove(from, index)?;
        task.touch();
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
        let brief: Vec<(ListId, &str, State)> = moved.iter().map(|(c, t)| (*c, t.text.as_str(), t.state)).collect();
        assert_eq!(brief, [(ListId::WORK, "a", State::Open), (ListId::HOME, "c", State::Open)]);
        // Копии и пометки «перенесена» — с временем правки, для слияния версий.
        assert!(moved.iter().all(|(_, t)| t.modified > 0) && board.list(ListId::WORK)[0].modified > 0);
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
