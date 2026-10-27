//! Горячие клавиши одной таблицей на экран (`ROWS` — главный, `NOTES_ROWS` — окно записей):
//! из неё и обработка нажатий (`App::act`, `App::act_notes`), и справка (`?`), и подсказки
//! внизу — так они не могут разойтись. У прочих всплывающих окон и поля ввода клавиши свои.

use ratatui::crossterm::event::{KeyCode, KeyModifiers};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Quit,
    Help,
    About,
    List(usize),
    NextList,
    PrevList,
    Down,
    Up,
    Top,
    Bottom,
    Add,
    Edit,
    Toggle,
    /// Отменена / снова открыта.
    Cancel,
    MoveUp,
    MoveDown,
    /// Перенос в другой список: дальше ждём цифру.
    MoveTo,
    /// Перенос на другую дату: спрашиваем дату.
    Postpone,
    Delete,
    PrevDay,
    NextDay,
    Today,
    GoToDate,
    /// Окно доп. записей задачи.
    Notes,
    /// В окне записей: новая подзадача.
    AddSubtask,
    /// В окне записей: файл записей во внешнем редакторе.
    OpenEditor,
    /// Закрыть окно записей.
    Close,
    /// Задача скрыта от ИИ / открыта.
    TogglePrivate,
    /// Список скрыт от ИИ / открыт.
    ToggleHidden,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Tasks,
    Pages,
    Navigation,
    Field,
    Notes,
}

impl Section {
    pub const ALL: [Section; 4] = [
        Section::Tasks,
        Section::Pages,
        Section::Navigation,
        Section::Field,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Tasks => "Задачи",
            Section::Pages => "Страницы",
            Section::Navigation => "Навигация",
            Section::Field => "Поле ввода",
            Section::Notes => "Записи задачи",
        }
    }
}

/// Клавиша с модификаторами. У символов Shift уже применён к самому символу (`G`, `?`),
/// модификатор остаётся только у прочих клавиш (`Shift+↑`).
pub type Key = (KeyCode, KeyModifiers);

/// Строка справки и клавиши за ней.
pub struct Row {
    pub section: Section,
    /// Клавиши, как их пишет справка, когда это короче полного списка (`1-4  Tab`).
    pub label: Option<&'static str>,
    /// Что строка говорит в справке; `None` — в справке не показывается.
    pub help: Option<&'static str>,
    pub keys: &'static [(Key, Action)],
}

use Action as A;

const NONE: KeyModifiers = KeyModifiers::NONE;
const SHIFT: KeyModifiers = KeyModifiers::SHIFT;

const fn c(c: char) -> Key {
    (KeyCode::Char(c), NONE)
}

const fn k(code: KeyCode) -> Key {
    (code, NONE)
}

const fn row(section: Section, help: &'static str, keys: &'static [(Key, Action)]) -> Row {
    Row {
        section,
        label: None,
        help: Some(help),
        keys,
    }
}

const fn labeled(
    section: Section,
    label: &'static str,
    help: &'static str,
    keys: &'static [(Key, Action)],
) -> Row {
    Row {
        section,
        label: Some(label),
        help: Some(help),
        keys,
    }
}

pub const ROWS: &[Row] = &[
    row(Section::Tasks, "добавить в конец списка", &[(c('a'), A::Add)]),
    row(
        Section::Tasks,
        "редактировать",
        &[(c('e'), A::Edit), (k(KeyCode::Enter), A::Edit)],
    ),
    row(
        Section::Tasks,
        "выполнено / не выполнено",
        &[(c(' '), A::Toggle), (c('x'), A::Toggle)],
    ),
    row(Section::Tasks, "отменить / вернуть", &[(c('c'), A::Cancel)]),
    labeled(
        Section::Tasks,
        "Shift+↑ ↓",
        "сдвинуть выше / ниже",
        &[
            ((KeyCode::Up, SHIFT), A::MoveUp),
            ((KeyCode::Down, SHIFT), A::MoveDown),
        ],
    ),
    labeled(
        Section::Tasks,
        "m 1-4",
        "перенести в другой список",
        &[(c('m'), A::MoveTo)],
    ),
    row(
        Section::Tasks,
        "перенести на другую дату",
        &[(c('>'), A::Postpone)],
    ),
    row(
        Section::Tasks,
        "удалить",
        &[(c('d'), A::Delete), (k(KeyCode::Delete), A::Delete)],
    ),
    row(Section::Tasks, "записи задачи", &[(c('n'), A::Notes)]),
    row(Section::Tasks, "скрыть задачу от ИИ / открыть", &[(c('p'), A::TogglePrivate)]),
    row(Section::Tasks, "скрыть список от ИИ / открыть", &[(c('P'), A::ToggleHidden)]),
    labeled(
        Section::Pages,
        "[ ]  PgUp PgDn",
        "предыдущий / следующий день",
        &[
            (c('['), A::PrevDay),
            (c(']'), A::NextDay),
            (k(KeyCode::PageUp), A::PrevDay),
            (k(KeyCode::PageDown), A::NextDay),
        ],
    ),
    row(Section::Pages, "сегодня", &[(c('t'), A::Today)]),
    row(Section::Pages, "перейти к дате", &[(c('D'), A::GoToDate)]),
    labeled(
        Section::Navigation,
        "1-4  Tab",
        "списки",
        &[
            (c('1'), A::List(0)),
            (c('2'), A::List(1)),
            (c('3'), A::List(2)),
            (c('4'), A::List(3)),
            (k(KeyCode::Tab), A::NextList),
            (k(KeyCode::BackTab), A::PrevList),
        ],
    ),
    labeled(
        Section::Navigation,
        "j k  ↑ ↓",
        "перемещение",
        &[
            (c('j'), A::Down),
            (c('k'), A::Up),
            (k(KeyCode::Down), A::Down),
            (k(KeyCode::Up), A::Up),
        ],
    ),
    labeled(
        Section::Navigation,
        "g  G",
        "в начало / в конец",
        &[
            (c('g'), A::Top),
            (c('G'), A::Bottom),
            (k(KeyCode::Home), A::Top),
            (k(KeyCode::End), A::Bottom),
        ],
    ),
    row(Section::Navigation, "о программе", &[(c('o'), A::About)]),
    row(Section::Navigation, "выход", &[(c('q'), A::Quit)]),
    // Справка сама говорит, как её открыть.
    Row {
        section: Section::Navigation,
        label: None,
        help: None,
        keys: &[(c('?'), A::Help), (k(KeyCode::F(1)), A::Help)],
    },
    // Клавиши поля ввода обрабатывает само поле, здесь они только для справки.
    labeled(Section::Field, "Enter  Esc", "сохранить / отмена", &[]),
    labeled(Section::Field, "Ctrl+W", "удалить слово", &[]),
    labeled(Section::Field, "Ctrl+A  Ctrl+E", "в начало / в конец строки", &[]),
    labeled(Section::Field, "заметка", "Enter — строка, Ctrl+S — сохранить", &[]),
    labeled(Section::Field, "", "сама сохраняется раз в 10 с; Esc — закрыть", &[]),
    labeled(Section::Field, "дата", "2026-10-27  27.10  +1  -3", &[]),
];

/// Окно записей задачи: свои клавиши, где можно — те же, что на главном экране.
pub const NOTES_ROWS: &[Row] = &[
    row(Section::Notes, "заметка (с датой и временем)", &[(c('a'), A::Add)]),
    row(Section::Notes, "подзадача", &[(c('s'), A::AddSubtask)]),
    row(
        Section::Notes,
        "править строку",
        &[(c('e'), A::Edit), (k(KeyCode::Enter), A::Edit)],
    ),
    row(
        Section::Notes,
        "подзадача выполнена / нет",
        &[(c(' '), A::Toggle), (c('x'), A::Toggle)],
    ),
    labeled(
        Section::Notes,
        "Shift+↑ ↓",
        "сдвинуть выше / ниже",
        &[
            ((KeyCode::Up, SHIFT), A::MoveUp),
            ((KeyCode::Down, SHIFT), A::MoveDown),
        ],
    ),
    row(
        Section::Notes,
        "удалить строку",
        &[(c('d'), A::Delete), (k(KeyCode::Delete), A::Delete)],
    ),
    row(Section::Notes, "открыть в редакторе ($EDITOR)", &[(c('E'), A::OpenEditor)]),
    labeled(
        Section::Notes,
        "j k  ↑ ↓  g G",
        "перемещение",
        &[
            (c('j'), A::Down),
            (c('k'), A::Up),
            (k(KeyCode::Down), A::Down),
            (k(KeyCode::Up), A::Up),
            (c('g'), A::Top),
            (c('G'), A::Bottom),
            (k(KeyCode::Home), A::Top),
            (k(KeyCode::End), A::Bottom),
        ],
    ),
    labeled(
        Section::Notes,
        "Esc  q  n",
        "закрыть",
        &[(k(KeyCode::Esc), A::Close), (c('q'), A::Close), (c('n'), A::Close)],
    ),
    Row {
        section: Section::Notes,
        label: None,
        help: None,
        keys: &[(c('?'), A::Help), (k(KeyCode::F(1)), A::Help)],
    },
];

/// Первая клавиша действия — её «нажимает» тап по подсказке.
pub fn first_key(rows: &[Row], action: Action) -> Option<Key> {
    rows.iter()
        .flat_map(|r| r.keys)
        .find(|(_, a)| *a == action)
        .map(|&(k, _)| k)
}

fn find(rows: &[Row], key: Key) -> Option<Action> {
    rows.iter()
        .flat_map(|r| r.keys)
        .find(|(k, _)| *k == key)
        .map(|&(_, a)| a)
}

/// Что делает клавиша на главном экране.
pub fn action(key: Key) -> Option<Action> {
    find(ROWS, key)
}

/// Что делает клавиша в окне записей.
pub fn notes_action(key: Key) -> Option<Action> {
    find(NOTES_ROWS, key)
}

/// Клавиша, как её пишет справка (`short` — как подсказки внизу).
fn key_name((code, mods): Key, short: bool) -> String {
    let name = match code {
        KeyCode::Char(' ') if short => "␣".into(),
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Left => "←".into(),
        KeyCode::Right => "→".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other}"),
    };
    match (mods.contains(SHIFT), short) {
        (true, true) => format!("⇧{name}"),
        (true, false) => format!("Shift+{name}"),
        (false, _) => name,
    }
}

/// Клавиши строки справки.
pub fn row_keys(row: &Row) -> String {
    match row.label {
        Some(label) => label.to_string(),
        None => row
            .keys
            .iter()
            .map(|&(k, _)| key_name(k, false))
            .collect::<Vec<_>>()
            .join("  "),
    }
}

/// Клавиша подсказки: первая из назначенных каждому из `actions`. Символы пишутся слитно
/// (`⇧↑↓`, общий `⇧` один раз), буквы — через косую черту (`n/b`).
pub fn hint_keys(actions: &[Action]) -> String {
    hint_keys_in(ROWS, actions)
}

/// То же для таблицы `rows` (окна записей).
pub fn hint_keys_in(rows: &[Row], actions: &[Action]) -> String {
    let names: Vec<String> = actions
        .iter()
        .filter_map(|a| rows.iter().flat_map(|r| r.keys).find(|(_, b)| b == a))
        .map(|&(k, _)| key_name(k, true))
        .collect();
    let symbols = names.iter().all(|n| !n.chars().any(char::is_alphanumeric));
    if !symbols {
        return names.join("/");
    }
    let all_shift = names.iter().all(|n| n.starts_with('⇧'));
    let mut out = String::new();
    for (i, name) in names.iter().enumerate() {
        out.push_str(if all_shift && i > 0 { &name['⇧'.len_utf8()..] } else { name });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        // Ни одна клавиша не делает двух дел — на каждом экране.
        for rows in [ROWS, NOTES_ROWS] {
            let all: Vec<Key> = rows.iter().flat_map(|r| r.keys).map(|&(k, _)| k).collect();
            for (i, k) in all.iter().enumerate() {
                assert!(!all[..i].contains(k), "{k:?} назначена дважды");
            }
        }
        assert_eq!(notes_action(c('s')), Some(A::AddSubtask));
        assert_eq!(notes_action(k(KeyCode::Esc)), Some(A::Close));
        assert_eq!(action(c('n')), Some(A::Notes));
        assert_eq!(hint_keys_in(NOTES_ROWS, &[A::OpenEditor]), "E");
        assert_eq!(action(c(' ')), Some(A::Toggle));
        assert_eq!(action(k(KeyCode::F(1))), Some(A::Help));
        assert_eq!(action((KeyCode::Up, SHIFT)), Some(A::MoveUp));
        assert_eq!(action(k(KeyCode::Up)), Some(A::Up));
        assert_eq!(action(c('J')), None);
        assert_eq!(action(c('K')), None);
        assert_eq!(row_keys(&ROWS[2]), "Space  x");
        assert_eq!(hint_keys(&[A::Toggle]), "␣");
        assert_eq!(hint_keys(&[A::MoveUp, A::MoveDown]), "⇧↑↓");
        assert_eq!(hint_keys(&[A::Top, A::Bottom]), "g/G");
        assert_eq!(hint_keys(&[A::PrevDay, A::NextDay]), "[]");
        // У каждой строки справки есть клавиши (или подпись к клавишам, назначенным в другом месте).
        for r in ROWS.iter().chain(NOTES_ROWS) {
            assert!(!r.keys.is_empty() || r.label.is_some(), "{:?}", r.help);
        }
    }
}
