use std::io::{self, Write};
use std::path::Path;

use directories::BaseDirs;
use ratatui::Frame;
use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::style::{Attribute, Print, ResetColor, SetAttribute, SetForegroundColor};
use ratatui::crossterm::queue;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, HighlightSpacing, List, ListItem, Padding, Paragraph, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, AUTOSAVE, DatePurpose, Draft, Hit, ListsMode, Mode, NoteTarget, NotesMode, NotesView, Setting, Status};
use crate::lists::MAX_LISTS;
use crate::clock::{self, Date};
use crate::field::LineInput;
use crate::keymap::{self, Action as A, Key, Row, Section};
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use crate::model::{ListId, State, Task};
use crate::field::TextArea;
use crate::notes::{Item, Kind};

const ACCENT: Color = Color::Cyan;
const SUB: Color = Color::Gray;
const MUTED: Color = Color::DarkGray;

pub const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// Уже этой ширины — телефон (Termux): вкладки и один список на экран вместо сетки 2×2.
pub const NARROW: u16 = 80;

/// С какой ширины экрана помещается панель превью рядом с сеткой.
pub const PREVIEW_MIN: u16 = 120;

/// Сетка для `n` списков: (колонок, рядов) — 2×2, 3×2, 3×3, 4×3, 4×4, 5×4, 5×5.
pub fn grid(n: usize) -> (usize, usize) {
    match n {
        0 | 1 => (1, 1),
        2 => (2, 1),
        3 | 4 => (2, 2),
        5 | 6 => (3, 2),
        7..=9 => (3, 3),
        10..=12 => (4, 3),
        13..=16 => (4, 4),
        17..=20 => (5, 4),
        _ => (5, 5),
    }
}

/// Карта попаданий кадра: где что лежит, чтобы тап попал куда надо (`App::on_mouse`).
type Hits = Vec<(Rect, Hit)>;

/// Строка из кусков, часть которых тапабельна: куски кладутся подряд от `area.x`.
fn place(area: Rect, parts: Vec<(Span<'static>, Option<Hit>)>, hits: &mut Hits) -> Line<'static> {
    let mut x = area.x;
    let mut spans = Vec::new();
    for (span, hit) in parts {
        let w = span.width() as u16;
        if let Some(hit) = hit
            && x < area.right()
        {
            hits.push((Rect::new(x, area.y, w.min(area.right() - x), 1), hit));
        }
        x = x.saturating_add(w);
        spans.push(span);
    }
    Line::from(spans)
}

fn key_hit(rows: &[Row], action: A) -> Option<Hit> {
    keymap::first_key(rows, action).map(Hit::Key)
}

/// Перенесённые — уже не дела этой страницы, отменённые — вовсе не дела, в счётчике их нет.
fn counts(tasks: &[Task]) -> (usize, usize) {
    let done = tasks.iter().filter(|t| t.state == State::Done).count();
    let total = tasks.iter().filter(|t| matches!(t.state, State::Open | State::Done)).count();
    (done, total)
}

/// Кликабельная ссылка (OSC 8) поверх уже нарисованного кадра, см. `write_links`.
pub struct Link {
    x: u16,
    y: u16,
    text: String,
    url: &'static str,
}

/// Рисует кадр; возвращает ссылки, которые надо дописать поверх него (`write_links`).
pub fn draw(frame: &mut Frame, app: &mut App) -> Vec<Link> {
    let mut hits = Hits::new();
    app.width = frame.area().width;
    let narrow = frame.area().width < NARROW;
    // На узком экране подсказки — в две строки: ими удобно пользоваться как кнопками.
    let [header_area, board_area, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(if narrow { 2 } else { 1 }),
    ])
    .areas(frame.area());
    draw_header(frame, app, header_area, narrow, &mut hits);

    if narrow {
        let [tabs, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(board_area);
        draw_tabs(frame, app, tabs, &mut hits);
        draw_list(frame, app, app.active, list, &mut hits);
    } else {
        let grid_area = if app.config.preview() && frame.area().width >= PREVIEW_MIN {
            let width = (board_area.width * 2 / 5).clamp(40, 70);
            let [grid_area, preview] =
                Layout::horizontal([Constraint::Min(0), Constraint::Length(width)]).areas(board_area);
            draw_preview(frame, app, preview, &mut hits);
            grid_area
        } else {
            board_area
        };
        // Сетка по числу списков, по рядам слева направо; неполный последний ряд — шире.
        let shown = app.shown.clone();
        let (cols, _) = grid(shown.len());
        let rows: Vec<&[ListId]> = shown.chunks(cols).collect();
        let row_areas = Layout::vertical(vec![Constraint::Ratio(1, rows.len() as u32); rows.len()]).split(grid_area);
        for (row, row_area) in rows.into_iter().zip(row_areas.iter()) {
            let cells = Layout::horizontal(vec![Constraint::Ratio(1, row.len() as u32); row.len()]).split(*row_area);
            for (&category, cell) in row.iter().zip(cells.iter()) {
                draw_list(frame, app, category, *cell, &mut hits);
            }
        }
    }

    draw_status(frame, app, status_area, &mut hits);

    match &app.mode {
        Mode::Edit { .. } => draw_edit(frame, app, &mut hits),
        Mode::Date { .. } => draw_date(frame, app, &mut hits),
        Mode::ConfirmDelete => draw_confirm(frame, app, &mut hits),
        &Mode::Settings(selected) => draw_settings(frame, app, selected, &mut hits),
        &Mode::MoveTo(selected) => {
            let items: Vec<String> = app.move_targets().iter().map(|&c| app.list_title(c).to_string()).collect();
            draw_pick(frame, "Перенести в список", &items, selected, &mut hits);
        }
        Mode::Lists { .. } => draw_lists(frame, app, &mut hits),
        Mode::Normal => {}
    }
    if app.notes.is_some() {
        draw_notes(frame, app, board_area, &mut hits);
    }
    if app.show_help {
        draw_help(frame, app.notes.is_some());
    }
    let links = if app.show_about { draw_about(frame) } else { Vec::new() };
    app.hits = hits;
    links
}

/// Дописывает ссылки в терминал напрямую. В буфер ratatui их не положить: unicode-width 0.2
/// считает ширину escape-последовательности вместе с URL, и ratatui пропускает ячейки за ней.
/// Текст ссылки в буфере уже есть, поэтому следующий кадр её не затрёт, пока окно открыто.
pub fn write_links(links: &[Link]) -> io::Result<()> {
    if links.is_empty() {
        return Ok(());
    }
    let mut out = io::stdout();
    for link in links {
        queue!(
            out,
            MoveTo(link.x, link.y),
            SetForegroundColor(ratatui::crossterm::style::Color::DarkCyan),
            SetAttribute(Attribute::Underlined),
            Print(format!("\x1b]8;;{}\x07{}\x1b]8;;\x07", link.url, link.text)),
            SetAttribute(Attribute::Reset),
            ResetColor,
        )?;
    }
    out.flush()
}

/// Путь для показа: домашний каталог — `~`.
fn display_path(path: &Path) -> String {
    let home = BaseDirs::new().map(|d| d.home_dir().to_path_buf());
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => Path::new("~").join(rest).display().to_string(),
        None => path.display().to_string(),
    }
}

/// Отрезает начало строки так, чтобы она поместилась в `width` колонок (`…/ztaskmgr/tasks.md`).
fn truncate_start(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 1;
    for c in text.chars().rev() {
        used += c.width().unwrap_or(0);
        if used > width {
            break;
        }
        out.insert(0, c);
    }
    out.insert(0, '…');
    out
}

/// Дата страницы целиком: «Понедельник, 26 октября 2026».
fn page_date(date: Date) -> String {
    date.format(c"%A, %-d %B %Y")
}

/// Дата, на которую перенесена задача: год — только если он не тот, что у страницы.
fn short_date(date: Date, page: Date) -> String {
    if date.year() == page.year() {
        date.format(c"%-d %B")
    } else {
        date.format(c"%-d %B %Y")
    }
}

/// Шапка: название, ◀ дата открытой страницы ▶ и каталог книжки слева, текущие дата и время
/// справа. Стрелки листают дни, тап по дате — сегодня. На узком экране — только дата и время.
fn draw_header(frame: &mut Frame, app: &App, area: Rect, narrow: bool, hits: &mut Hits) {
    let now = if narrow { clock::hm() } else { clock::now() };
    let right = Line::from(format!("{now} ").fg(SUB));
    let relative = match app.today.days_until(app.date) {
        0 => " · сегодня",
        -1 => " · вчера",
        1 => " · завтра",
        _ => "",
    };
    let date = if narrow { app.date.format(c"%a, %-d %b") } else { page_date(app.date) };
    let mut parts: Vec<(Span<'static>, Option<Hit>)> = Vec::new();
    if !narrow {
        parts.push((Span::raw(" ≡ ").fg(Color::Black).bg(ACCENT).bold(), None));
        parts.push((Span::raw(" ztaskmgr ").fg(ACCENT).bold(), None));
    }
    parts.extend([
        (Span::raw(" ◀ ").fg(ACCENT), key_hit(keymap::ROWS, A::PrevDay)),
        (Span::raw(date).bold(), key_hit(keymap::ROWS, A::Today)),
        (Span::raw(relative).fg(MUTED), key_hit(keymap::ROWS, A::Today)),
        (Span::raw(" ▶ ").fg(ACCENT), key_hit(keymap::ROWS, A::NextDay)),
    ]);
    if !narrow {
        let used: usize = parts.iter().map(|(s, _)| s.width()).sum();
        // Между путём и часами — хотя бы два пробела; не помещается — путь обрезается слева.
        let room = usize::from(area.width).saturating_sub(used + 2 + right.width() + 2);
        let path = truncate_start(&display_path(app.book.dir()), room);
        parts.push((Span::raw("  "), None));
        parts.push((Span::raw(path).fg(SUB), None));
    }
    frame.render_widget(place(area, parts, hits), area);
    frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
}

/// Вкладки списков на узком экране: тап — список. Не помещаются — у неактивных только номера.
fn draw_tabs(frame: &mut Frame, app: &App, area: Rect, hits: &mut Hits) {
    let tab = |i: usize, c: ListId, full: bool| {
        let lock = if app.is_hidden(c) { " 🔒" } else { "" };
        let title = app.list_title(c);
        if c == app.active {
            let (done, total) = counts(app.board.list(c));
            let label = format!(" {} {title}{lock} {done}/{total} ", i + 1);
            Span::raw(label).fg(Color::Black).bg(ACCENT).bold()
        } else if full {
            Span::raw(format!(" {} {title}{lock} ", i + 1)).fg(SUB)
        } else {
            Span::raw(format!(" {} ", i + 1)).fg(SUB)
        }
    };
    let shown = &app.shown;
    let width = |full: bool| -> usize { shown.iter().enumerate().map(|(i, &c)| tab(i, c, full).width()).sum() };
    let full = width(true) <= usize::from(area.width);
    let tabs: Vec<Span<'static>> = shown.iter().enumerate().map(|(i, &c)| tab(i, c, full)).collect();
    // Не помещаются и номерами — видны вкладки вокруг активной, по краям ‹ › к соседним.
    let active = shown.iter().position(|&c| c == app.active).unwrap_or(0);
    let room = usize::from(area.width).saturating_sub(2);
    let (mut lo, mut hi) = (active, active + 1);
    let mut used = tabs[active].width();
    loop {
        let grew_right = hi < tabs.len() && used + tabs[hi].width() <= room;
        if grew_right {
            used += tabs[hi].width();
            hi += 1;
        }
        let grew_left = lo > 0 && used + tabs[lo - 1].width() <= room;
        if grew_left {
            lo -= 1;
            used += tabs[lo].width();
        }
        if !grew_right && !grew_left {
            break;
        }
    }
    let mut parts: Vec<(Span<'static>, Option<Hit>)> = Vec::new();
    if lo > 0 {
        parts.push(("‹".fg(SUB), Some(Hit::List(shown[lo - 1]))));
    }
    for i in lo..hi {
        parts.push((tabs[i].clone(), Some(Hit::List(shown[i]))));
    }
    if hi < tabs.len() {
        parts.push(("›".fg(SUB), Some(Hit::List(shown[hi]))));
    }
    frame.render_widget(place(area, parts, hits), area);
}

/// Окно с рамкой и заголовком, как у всех всплывающих окон.
fn panel(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Line::from(format!(" {} ", title.into()).bold()))
}

/// Нижняя подпись окон, которые закрываются любой клавишей.
fn any_key_closes() -> Line<'static> {
    Line::from(" любая клавиша — закрыть ".fg(MUTED)).centered()
}

fn draw_list(frame: &mut Frame, app: &mut App, category: ListId, area: Rect, hits: &mut Hits) {
    hits.push((area, Hit::List(category)));
    let active = app.active == category;
    let tasks = app.board.list(category);
    let (done, total) = counts(tasks);

    let border_style = if active {
        Style::new().fg(ACCENT).bold()
    } else {
        Style::new().fg(MUTED)
    };
    // Номер — у первых девяти: его клавиша выбирает список.
    let number = app.shown.iter().position(|&c| c == category).filter(|&i| i < 9).map(|i| format!("{} ", i + 1));
    let archived = !app.registry.is_active(category);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(format!(
            " {}{}{}{} ",
            number.unwrap_or_default(),
            app.list_title(category),
            if app.is_hidden(category) { " 🔒" } else { "" },
            if archived { " · архив" } else { "" }
        ))
        .title(Line::from(format!(" {done}/{total} ")).right_aligned());

    if tasks.is_empty() {
        let hint = Paragraph::new("пусто".fg(MUTED)).centered().block(block);
        frame.render_widget(hint, area);
        return;
    }

    let items: Vec<ListItem> = tasks
        .iter()
        .map(|task| {
            let mut line: Line = match task.state {
                State::Open => Line::raw(format!("[ ] {}", task.text)),
                State::Done => Line::from(vec![
                    Span::raw("[x] ").fg(MUTED),
                    Span::raw(task.text.as_str()).fg(MUTED).crossed_out(),
                ]),
                State::Cancelled => Line::from(vec![
                    Span::raw("[-] ").fg(MUTED),
                    Span::raw(task.text.as_str()).fg(MUTED).crossed_out().italic(),
                    Span::raw(" ✗").fg(MUTED),
                ]),
                State::Migrated(to) => Line::from(vec![
                    Span::raw("[>] ").fg(MUTED),
                    Span::raw(task.text.as_str()).fg(MUTED),
                    Span::raw(format!(" → {}", short_date(to, app.date))).fg(MUTED).italic(),
                ]),
            };
            if let Some(mark) = notes_mark(app, task) {
                line.push_span(Span::raw(mark).fg(SUB));
            }
            // В скрытом списке скрыто всё, замок у каждой задачи не нужен.
            if task.private && !app.is_hidden(category) {
                line.push_span(Span::raw(" 🔒").fg(SUB));
            }
            line.into()
        })
        .collect();

    let (symbol, highlight) = if active {
        ("› ", Style::new().reversed())
    } else {
        ("  ", Style::new())
    };
    let inner = block.inner(area);
    let len = tasks.len();
    let list = List::new(items)
        .block(block)
        .highlight_symbol(symbol)
        .highlight_style(highlight)
        .highlight_spacing(HighlightSpacing::Always);

    let state = app.lists.entry(category).or_default();
    frame.render_stateful_widget(list, area, state);
    // Задачи — по строке; `[ ]` — после символа выделения (две колонки).
    for (row, index) in (state.offset()..len).take(usize::from(inner.height)).enumerate() {
        let y = inner.y + row as u16;
        hits.push((Rect::new(inner.x, y, inner.width, 1), Hit::Task(category, index)));
        hits.push((Rect::new(inner.x + 2, y, 3.min(inner.width.saturating_sub(2)), 1), Hit::Check(category, index)));
    }
}

/// Значок доп. записей у задачи: « ✎», с подзадачами — « ✎ 1/3».
fn notes_mark(app: &App, task: &Task) -> Option<String> {
    let summary = app.summaries.get(task.id.as_deref()?)?;
    Some(if summary.subtasks > 0 {
        format!(" ✎ {}/{}", summary.subtasks_done, summary.subtasks)
    } else {
        " ✎".to_string()
    })
}

/// Подсказка внизу: клавиша, что делает и что «нажимает» тап по ней.
struct Hint {
    key: String,
    desc: &'static str,
    press: Option<Key>,
}

impl Hint {
    fn new(key: &str, desc: &'static str, code: KeyCode) -> Self {
        Self::with(key, desc, code, KeyModifiers::NONE)
    }

    fn with(key: &str, desc: &'static str, code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self {
            key: key.into(),
            desc,
            press: Some((code, modifiers)),
        }
    }

    fn width(&self) -> usize {
        self.key.width() + 1 + self.desc.width()
    }
}

/// Подсказки к действиям из таблицы клавиш экрана.
fn action_hints(rows: &[Row], actions: &[(&[A], &'static str)]) -> Vec<Hint> {
    actions
        .iter()
        .map(|(a, desc)| Hint {
            key: keymap::hint_keys_in(rows, a),
            desc,
            press: keymap::first_key(rows, a[0]),
        })
        .collect()
}

/// Окно выбора: цифра или Enter — выбрать, Esc — отмена.
fn pick_hints(enter: &'static str) -> Vec<Hint> {
    vec![
        Hint::new("1-9", "выбрать", KeyCode::Char('1')),
        Hint::new("j/k", "выбор", KeyCode::Down),
        Hint::new("Enter", enter, KeyCode::Enter),
        Hint::new("Esc", "отмена", KeyCode::Esc),
    ]
}

fn enter_esc(enter: &'static str, esc: &'static str) -> Vec<Hint> {
    vec![Hint::new("Enter", enter, KeyCode::Enter), Hint::new("Esc", esc, KeyCode::Esc)]
}

fn yes_other(yes: &'static str, other: &'static str) -> Vec<Hint> {
    vec![
        Hint::new("y/Enter", yes, KeyCode::Char('y')),
        Hint::new("другая", other, KeyCode::Esc),
    ]
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect, hits: &mut Hits) {
    match &app.status {
        Some(Status::Error(text)) => {
            let text = Paragraph::new(format!(" {text}").red()).wrap(Wrap { trim: true });
            frame.render_widget(text, area);
            return;
        }
        Some(Status::Info(text)) => {
            let text = Paragraph::new(format!(" {text}").fg(ACCENT)).wrap(Wrap { trim: true });
            frame.render_widget(text, area);
            return;
        }
        None => {}
    }
    if let Some(view) = &app.notes {
        draw_notes_status(frame, view, area, hits);
        return;
    }
    let hints = match &app.mode {
        Mode::Normal => action_hints(
            keymap::ROWS,
            &[
                (&[A::Add], "добавить"),
                (&[A::Edit], "правка"),
                (&[A::Toggle], "готово"),
                (&[A::PrevDay, A::NextDay], "день"),
                (&[A::Postpone], "на дату"),
                (&[A::MoveUp, A::MoveDown], "сдвиг"),
                (&[A::MoveTo], "перенос"),
                (&[A::Notes], "записи"),
                (&[A::Delete], "удалить"),
                (&[A::Help], "помощь"),
                (&[A::Quit], "выход"),
            ],
        ),
        Mode::Edit { .. } => enter_esc("сохранить", "отмена"),
        Mode::Date { purpose, .. } => match purpose {
            DatePurpose::Go => enter_esc("перейти", "отмена"),
            DatePurpose::Postpone(_) => enter_esc("перенести", "отмена"),
        },
        Mode::ConfirmDelete => yes_other("удалить", "отмена"),
        Mode::Settings(_) => vec![
            Hint::new("␣", "переключить", KeyCode::Char(' ')),
            Hint::new("j/k", "выбор", KeyCode::Down),
            Hint::new("Esc", "закрыть", KeyCode::Esc),
        ],
        Mode::MoveTo(_) => pick_hints("перенести"),
        Mode::Lists { sub, .. } => match sub {
            ListsMode::Browse => vec![
                Hint::new("a", "добавить", KeyCode::Char('a')),
                Hint::new("e", "переименовать", KeyCode::Char('e')),
                Hint::new("d", "удалить", KeyCode::Char('d')),
                Hint::with("⇧↑↓", "порядок", KeyCode::Up, KeyModifiers::SHIFT),
                Hint::new("p", "скрыть от ИИ", KeyCode::Char('p')),
                Hint::new("r", "вернуть из архива", KeyCode::Char('r')),
                Hint::new("Esc", "закрыть", KeyCode::Esc),
            ],
            ListsMode::Add(_) | ListsMode::Rename(_) => enter_esc("сохранить", "отмена"),
            ListsMode::Delete { count: 0, .. } => yes_other("удалить", "отмена"),
            ListsMode::Delete { .. } => pick_hints("перенести и удалить"),
        },
    };
    let main = matches!(app.mode, Mode::Normal);
    // Как закрыть окно — видно всегда, даже если остальное не влезло.
    draw_hint_line(frame, area, &hints, &[&keymap::hint_keys(&[A::Help]), "Esc"], main, hits);
}

fn draw_notes_status(frame: &mut Frame, view: &NotesView, area: Rect, hits: &mut Hits) {
    let hints = match view.mode {
        NotesMode::List => action_hints(
            keymap::NOTES_ROWS,
            &[
                (&[A::Add], "заметка"),
                (&[A::AddSubtask], "подзадача"),
                (&[A::Edit], "правка"),
                (&[A::Toggle], "готово"),
                (&[A::MoveUp, A::MoveDown], "сдвиг"),
                (&[A::Delete], "удалить"),
                (&[A::OpenEditor], "редактор"),
                (&[A::Help], "помощь"),
                (&[A::Close], "закрыть"),
            ],
        ),
        NotesMode::Edit { .. } => enter_esc("сохранить", "отмена"),
        NotesMode::Text(Draft { confirm_close: true, .. }) => vec![
            Hint::new("y/Enter", "закрыть", KeyCode::Char('y')),
            Hint::new("s/Ctrl+S", "сохранить и закрыть", KeyCode::Char('s')),
            Hint::new("другая", "вернуться", KeyCode::Esc),
        ],
        NotesMode::Text(_) => vec![
            Hint::with("Ctrl+S", "сохранить", KeyCode::Char('s'), KeyModifiers::CONTROL),
            Hint::new("Enter", "новая строка", KeyCode::Enter),
            Hint::new("Esc", "закрыть", KeyCode::Esc),
        ],
        NotesMode::ConfirmDelete => yes_other("удалить", "отмена"),
    };
    // Без «закрыть» из окна на телефоне не выйти — она остаётся вместе со справкой.
    let help = keymap::hint_keys_in(keymap::NOTES_ROWS, &[A::Help]);
    let close = keymap::hint_keys_in(keymap::NOTES_ROWS, &[A::Close]);
    draw_hint_line(frame, area, &hints, &[&help, &close, "Esc"], false, hits);
}

/// Окно доп. записей задачи: подзадачи чекбоксами, заметки с датой, прочие строки как есть.
fn draw_notes(frame: &mut Frame, app: &mut App, board: Rect, hits: &mut Hits) {
    let Some(title) = app.notes_task().map(|t| format!("Записи · {}", t.text)) else {
        return;
    };
    let Some(view) = &mut app.notes else {
        return;
    };
    let screen = frame.area();
    // На узком экране — на всё место под шапкой, иначе окном по центру.
    let area = if screen.width < NARROW {
        board
    } else {
        centered(screen, (screen.width * 4 / 5).clamp(40, 100), (screen.height * 3 / 4).max(8))
    };
    hits.push((area, Hit::Block));
    let mut block = panel(title).padding(Padding::horizontal(1));
    let summary = view.notes.summary();
    if summary.subtasks > 0 {
        block = block.title(
            Line::from(format!(" подзадачи {}/{} ", summary.subtasks_done, summary.subtasks))
                .right_aligned(),
        );
    }
    let visible = view.notes.visible();
    if visible.is_empty() {
        block = block.title_bottom(
            Line::from(" пусто — a: заметка, s: подзадача, E: редактор ".fg(MUTED)).centered(),
        );
    }
    let inner = block.inner(area);
    // Отступ под символ выделения.
    let text_width = usize::from(inner.width).saturating_sub(2).max(10);

    // Высота каждого пункта в строках и подзадача ли он — для карты попаданий.
    let mut shapes: Vec<(usize, bool)> = Vec::new();
    let items: Vec<ListItem> = visible
        .iter()
        .map(|&i| {
            let item = view.notes.item(i);
            let subtask = matches!(item.kind, Kind::Subtask { .. });
            let lines = entry_lines(&item, text_width);
            shapes.push((lines.len(), subtask));
            ListItem::new(lines)
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_symbol("› ")
        .highlight_style(Style::new().reversed())
        .highlight_spacing(HighlightSpacing::Always);
    frame.render_widget(Clear, area);
    frame.render_stateful_widget(list, area, &mut view.list);
    let mut y = inner.y;
    'items: for (n, &(height, subtask)) in shapes.iter().enumerate().skip(view.list.offset()) {
        for line in 0..height {
            if y >= inner.bottom() {
                break 'items;
            }
            hits.push((Rect::new(inner.x, y, inner.width, 1), Hit::Entry(n)));
            if line == 0 && subtask {
                hits.push((Rect::new(inner.x + 2, y, 3, 1), Hit::EntryCheck(n)));
            }
            y += 1;
        }
    }

    match &mut view.mode {
        NotesMode::List => {}
        NotesMode::Edit { input, target, .. } => {
            let title = match target {
                NoteTarget::Entry(_) => "Правка подзадачи",
                _ => "Новая подзадача",
            };
            draw_field(frame, panel(title), input, hits);
        }
        NotesMode::Text(draft) => {
            let title = match draft.target {
                NoteTarget::Note => "Новая заметка",
                _ => "Заметка",
            };
            let saved = match &draft.saved_label {
                Some(time) if !draft.is_changed() => format!(" сохранено в {time} "),
                Some(time) => format!(" сохранено в {time} · автосохранение раз в {} с ", AUTOSAVE.as_secs()),
                None => format!(" автосохранение раз в {} с ", AUTOSAVE.as_secs()),
            };
            draw_text_area(frame, title, &saved, &mut draft.area, hits);
            if draft.confirm_close {
                let answers = Line::from(vec![
                    Span::raw("y").red().bold(),
                    Span::raw(" — закрыть без них · ").fg(MUTED),
                    Span::raw("s").fg(ACCENT).bold(),
                    Span::raw(" — сохранить · другая — назад").fg(MUTED),
                ]);
                draw_question_with(
                    frame,
                    "Закрыть заметку?",
                    "Последние изменения ещё не сохранены.",
                    None,
                    answers,
                    hits,
                );
            }
        }
        NotesMode::ConfirmDelete => {
            let text = view.selected_entry().map(|i| view.notes.editable_text(i)).unwrap_or_default();
            // В вопросе — первая строка пункта.
            let first = text.lines().next().unwrap_or("");
            draw_question(frame, "Удаление", &format!("Удалить «{first}»?"), None, hits);
        }
    }
}

/// Строки пункта записей: префикс (`[ ]` или метка времени), текст по словам не шире `width`,
/// продолжение — с отступом под префикс.
fn entry_lines(item: &Item, width: usize) -> Vec<Line<'static>> {
    let (prefix, style): (Vec<Span>, Style) = match item.kind {
        Kind::Subtask { done: false } => (vec![Span::raw("[ ] ")], Style::new()),
        Kind::Subtask { done: true } => (
            vec![Span::raw("[x] ").fg(MUTED)],
            Style::new().fg(MUTED).crossed_out(),
        ),
        Kind::Note { stamp } => (vec![Span::raw(format!("{stamp}  ")).fg(MUTED)], Style::new()),
        Kind::Text => (vec![], Style::new()),
    };
    let indent: usize = prefix.iter().map(Span::width).sum();
    let wrap_width = width.saturating_sub(indent).max(5);
    std::iter::once(item.head)
        .chain(item.body.iter().copied())
        .flat_map(|line| wrap(line, wrap_width))
        .enumerate()
        .map(|(n, chunk)| {
            let mut spans = if n == 0 {
                prefix.clone()
            } else {
                vec![Span::raw(" ".repeat(indent))]
            };
            spans.push(Span::styled(chunk, style));
            Line::from(spans)
        })
        .collect()
}

fn state_mark(state: State) -> &'static str {
    match state {
        State::Open => "[ ]",
        State::Done => "[x]",
        State::Cancelled => "[-]",
        State::Migrated(_) => "[>]",
    }
}

/// Панель превью справа: выделенная задача целиком — текст, состояние, история по страницам,
/// записи. Только просмотр; тап — окно записей, свайп и J/K — прокрутка.
fn draw_preview(frame: &mut Frame, app: &mut App, area: Rect, hits: &mut Hits) {
    hits.push((area, Hit::Preview));
    let active = app.notes.is_none() && matches!(app.mode, Mode::Normal);
    let block = panel("Превью")
        .border_style(Style::new().fg(if active { SUB } else { MUTED }))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    let width = usize::from(inner.width).max(10);
    let (date, category) = (app.date, app.active);
    let Some(task) = app.selected_task().cloned() else {
        let empty = Paragraph::new("нет задачи".fg(MUTED)).centered().block(block);
        frame.render_widget(empty, area);
        return;
    };

    let mut lines: Vec<Line> = wrap(&task.text, width)
        .into_iter()
        .map(|l| Line::from(Span::raw(l).bold()))
        .collect();
    let state = match task.state {
        State::Open => "открыта".to_string(),
        State::Done => "выполнена".to_string(),
        State::Cancelled => "отменена".to_string(),
        State::Migrated(to) => format!("перенесена на {}", short_date(to, date)),
    };
    let mut meta = vec![Span::raw(state).fg(SUB), Span::raw(format!(" · {}", app.list_title(category))).fg(MUTED)];
    let registry = app.registry.clone();
    if task.private {
        meta.push(Span::raw(" · 🔒 скрыта от ИИ").fg(MUTED));
    }
    lines.push(Line::from(meta));

    if let Some(preview) = app.preview() {
        // История — если задача побывала больше чем на одной странице.
        if preview.history.len() > 1 {
            lines.push(Line::default());
            lines.push(Line::from("История".fg(ACCENT)));
            for &(day, list, state) in &preview.history {
                let mut spans = vec![
                    Span::raw(format!("{} ", state_mark(state))).fg(MUTED),
                    Span::raw(short_date(day, date)),
                ];
                if list != category {
                    spans.push(Span::raw(format!(" · {}", registry.label(list))).fg(MUTED));
                }
                if day == date {
                    spans.push(Span::raw(" ← эта страница").fg(MUTED));
                }
                lines.push(Line::from(spans));
            }
        }
        let visible = preview.notes.visible();
        if !visible.is_empty() {
            lines.push(Line::default());
            let summary = preview.notes.summary();
            let mut title = vec![Span::raw("Записи").fg(ACCENT)];
            if summary.subtasks > 0 {
                title.push(Span::raw(format!(" · подзадачи {}/{}", summary.subtasks_done, summary.subtasks)).fg(MUTED));
            }
            lines.push(Line::from(title));
            for i in visible {
                lines.extend(entry_lines(&preview.notes.item(i), width));
            }
        }
    }

    let max_scroll = lines.len().saturating_sub(usize::from(inner.height)) as u16;
    app.preview_scroll = app.preview_scroll.min(max_scroll);
    let mut block = block;
    if max_scroll > 0 {
        let more = match (app.preview_scroll > 0, app.preview_scroll < max_scroll) {
            (true, true) => " J ↓ · K ↑ ",
            (false, _) => " J ↓ ",
            (true, false) => " K ↑ ",
        };
        block = block.title_bottom(Line::from(more.fg(MUTED)).right_aligned());
    }
    frame.render_widget(Paragraph::new(lines).block(block).scroll((app.preview_scroll, 0)), area);
}

/// Окно выбора из списка: номера у первых девяти, выделенный вариант, тап — выбрать.
fn draw_pick(frame: &mut Frame, title: &str, items: &[String], selected: usize, hits: &mut Hits) {
    let widest = items.iter().map(|t| t.width()).max().unwrap_or(0).max(title.width() + 2) as u16;
    let r = popup_area(frame.area(), widest + 10, items.len() as u16 + 2);
    hits.push((r, Hit::Block));
    let block = panel(title).padding(Padding::horizontal(1));
    let inner = block.inner(r);
    let rows: Vec<ListItem> = items
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let number = if i < 9 { format!("{} ", i + 1) } else { "  ".into() };
            ListItem::new(Line::from(vec![Span::raw(number).fg(MUTED), Span::raw(text.clone())]))
        })
        .collect();
    let list = List::new(rows).block(block).highlight_style(Style::new().reversed());
    let mut state = ratatui::widgets::ListState::default().with_selected(Some(selected));
    frame.render_widget(Clear, r);
    frame.render_stateful_widget(list, r, &mut state);
    for (row, i) in (state.offset()..items.len()).take(usize::from(inner.height)).enumerate() {
        hits.push((Rect::new(inner.x, inner.y + row as u16, inner.width, 1), Hit::Pick(i)));
    }
}

/// Окно списков: те, что на экране, по порядку, затем архивные; поверх — поле названия,
/// вопрос или выбор, куда перенести задачи удаляемого списка.
fn draw_lists(frame: &mut Frame, app: &App, hits: &mut Hits) {
    let Mode::Lists { row, sub } = &app.mode else {
        return;
    };
    let rows = app.list_rows();
    let row = (*row).min(rows.len().saturating_sub(1));
    let items: Vec<ListItem> = rows
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let archived = !app.registry.is_active(c);
            let number = if i < 9 && !archived { format!("{} ", i + 1) } else { "  ".into() };
            let mut spans = vec![Span::raw(number).fg(MUTED)];
            spans.push(if archived { Span::raw(app.list_title(c).to_string()).fg(MUTED) } else { Span::raw(app.list_title(c).to_string()) });
            if app.is_hidden(c) {
                spans.push(Span::raw(" 🔒").fg(SUB));
            }
            if archived {
                spans.push(Span::raw(" · архив").fg(MUTED));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let title = format!("Списки · {} из {MAX_LISTS}", app.registry.active().len());
    let r = popup_area(frame.area(), 50, rows.len() as u16 + 2);
    hits.push((r, Hit::Block));
    let block = panel(title).padding(Padding::horizontal(1));
    let inner = block.inner(r);
    let list = List::new(items).block(block).highlight_style(Style::new().reversed());
    let mut state = ratatui::widgets::ListState::default().with_selected(Some(row));
    frame.render_widget(Clear, r);
    frame.render_stateful_widget(list, r, &mut state);
    for (y, i) in (state.offset()..rows.len()).take(usize::from(inner.height)).enumerate() {
        hits.push((Rect::new(inner.x, inner.y + y as u16, inner.width, 1), Hit::Pick(i)));
    }

    let Some(&list) = rows.get(row) else {
        return;
    };
    let name = app.list_title(list);
    match sub {
        ListsMode::Browse => {}
        ListsMode::Add(input) => draw_field(frame, panel("Новый список"), input, hits),
        ListsMode::Rename(input) => draw_field(frame, panel(format!("Переименовать «{name}»")), input, hits),
        ListsMode::Delete { count: 0, .. } => {
            draw_question(frame, "Удаление", &format!("Удалить список «{name}»?"), None, hits);
        }
        &ListsMode::Delete { count, target } => {
            let items: Vec<String> = app.delete_targets(list).iter().map(|&c| app.list_title(c).to_string()).collect();
            draw_pick(frame, &format!("Задачи «{name}» ({count}) — в список"), &items, target, hits);
        }
    }
}

/// Окно настроек этого устройства: переключатели, пробел или тап — переключить.
fn draw_settings(frame: &mut Frame, app: &App, selected: usize, hits: &mut Hits) {
    let rows = Setting::ALL.len() as u16 * 2;
    let r = popup_area(frame.area(), 60, rows + 4);
    hits.push((r, Hit::Block));
    let mut block = panel("Настройки").padding(Padding::uniform(1));
    if let Some(path) = app.config.path() {
        block = block.title_bottom(Line::from(format!(" {} ", display_path(path)).fg(MUTED)).right_aligned());
    }
    let inner = block.inner(r);
    let mut lines = Vec::new();
    for (i, setting) in Setting::ALL.into_iter().enumerate() {
        let mark = if setting.is_on(&app.config) { "[x] " } else { "[ ] " };
        let mut line = Line::from(vec![Span::raw(mark), Span::raw(setting.title())]);
        if i == selected {
            line = line.reversed();
        }
        lines.push(line);
        lines.push(Line::from(format!("    {}", setting.help()).fg(MUTED)));
        let y = inner.y + 2 * i as u16;
        if y < inner.bottom() {
            hits.push((Rect::new(inner.x, y, inner.width, 2.min(inner.bottom() - y)), Hit::Setting(i)));
        }
    }
    frame.render_widget(Clear, r);
    frame.render_widget(Paragraph::new(lines).block(block), r);
}

/// Многострочное поле: высота по тексту (от 3 до 12 строк), прокрутка за курсором.
fn draw_text_area(frame: &mut Frame, title: &str, saved: &str, area: &mut TextArea, hits: &mut Hits) {
    let screen = frame.area();
    let width = 70.min(screen.width.saturating_sub(4)).max(20);
    let probe = panel("").inner(Rect::new(0, 0, width, 3));
    area.set_width(usize::from(probe.width));
    let rows = area.rows();
    let max_rows = 12.min(screen.height.saturating_sub(6)).max(3);
    let height = (rows.len() as u16).clamp(3, max_rows);
    let rect = centered(screen, width, height + 2);
    hits.push((rect, Hit::Block));
    let block = panel(title)
        .title_bottom(Line::from(" Ctrl+S — сохранить · Esc — закрыть ".fg(MUTED)))
        .title_bottom(Line::from(saved.to_string().fg(MUTED)).right_aligned());
    let inner = block.inner(rect);

    let (row, col) = area.cursor_pos();
    let offset = row.saturating_sub(usize::from(inner.height) - 1);
    let lines: Vec<Line> = rows
        .iter()
        .skip(offset)
        .take(usize::from(inner.height))
        .map(|&(start, end)| Line::raw(&area.text()[start..end]))
        .collect();
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
    frame.set_cursor_position((inner.x + col as u16, inner.y + (row - offset) as u16));
}

/// Перенос по словам в строки не шире `width` колонок; слишком длинные слова режутся.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in text.split(' ') {
        let current = lines.last_mut().unwrap();
        let needed = if current.is_empty() { word.width() } else { current.width() + 1 + word.width() };
        if needed <= width {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
            continue;
        }
        if !current.is_empty() {
            lines.push(String::new());
        }
        for c in word.chars() {
            let current = lines.last_mut().unwrap();
            if current.width() + c.width().unwrap_or(0) > width {
                lines.push(String::new());
            }
            lines.last_mut().unwrap().push(c);
        }
    }
    lines
}

/// Между подсказками.
const HINT_GAP: usize = 3;

/// Раскладывает подсказки по строкам `area` (на узком экране их две); первая строка короче
/// на `right` колонок. Возвращает (строка, колонка) каждой или `None`, если не все влезли.
fn lay_out(hints: &[&Hint], area: Rect, right: usize) -> Option<Vec<(u16, usize)>> {
    let mut places = Vec::new();
    let (mut row, mut x) = (0u16, 1usize);
    for hint in hints {
        loop {
            let limit = usize::from(area.width).saturating_sub(if row == 0 { right } else { 0 });
            if x + hint.width() <= limit {
                break;
            }
            row += 1;
            x = 1;
            if row >= area.height {
                return None;
            }
        }
        places.push((row, x));
        x += hint.width() + HINT_GAP;
    }
    Some(places)
}

/// Подсказки, обрезанные с конца по месту (подсказки `keep` — справка, «закрыть» — остаются),
/// и «о программе» у правого края на главном экране. Тап по подсказке — её клавиша.
fn draw_hint_line(frame: &mut Frame, area: Rect, hints: &[Hint], keep: &[&str], main: bool, hits: &mut Hits) {
    let about_key = keymap::hint_keys(&[A::About]);
    let about = main.then(|| {
        Line::from(vec![
            Span::raw(about_key.clone()).fg(ACCENT).bold(),
            Span::raw(" о программе ").fg(MUTED),
        ])
    });
    let right_w = about.as_ref().map_or(0, Line::width);
    let mut shown: Vec<&Hint> = hints.iter().collect();
    let places = loop {
        if let Some(places) = lay_out(&shown, area, right_w) {
            break places;
        }
        match shown.iter().rposition(|h| !keep.contains(&h.key.as_str())) {
            Some(last) => {
                shown.remove(last);
            }
            None => break lay_out(&shown, area, right_w).unwrap_or_default(),
        }
    };
    let mut lines: Vec<Vec<Span>> = vec![Vec::new(); usize::from(area.height)];
    let mut ends = vec![0usize; lines.len()];
    for (hint, &(row, x)) in shown.iter().zip(&places) {
        let line = &mut lines[usize::from(row)];
        line.push(Span::raw(" ".repeat(x - ends[usize::from(row)])));
        line.push(Span::raw(hint.key.clone()).fg(ACCENT).bold());
        line.push(Span::raw(format!(" {}", hint.desc)).fg(MUTED));
        ends[usize::from(row)] = x + hint.width();
        if let Some(press) = hint.press {
            let rect = Rect::new(area.x + x as u16, area.y + row, hint.width() as u16, 1);
            hits.push((rect.intersection(area), Hit::Key(press)));
        }
    }
    let lines: Vec<Line> = lines.into_iter().map(Line::from).collect();
    frame.render_widget(Paragraph::new(lines), area);
    if let Some(about) = about {
        let w = about.width() as u16;
        let rect = Rect::new(area.right().saturating_sub(w), area.y, w.min(area.width), 1);
        if let Some(key) = keymap::first_key(keymap::ROWS, A::About) {
            hits.push((rect, Hit::Key(key)));
        }
        frame.render_widget(Paragraph::new(about).alignment(Alignment::Right), area);
    }
}

fn draw_edit(frame: &mut Frame, app: &App, hits: &mut Hits) {
    let Mode::Edit { input, index } = &app.mode else {
        return;
    };
    let action = if index.is_some() { "Правка" } else { "Новая задача" };
    draw_field(frame, panel(format!("{action} · {}", app.list_title(app.active))), input, hits);
}

/// Окно даты; внизу — во что разбирается введённое.
fn draw_date(frame: &mut Frame, app: &App, hits: &mut Hits) {
    let Mode::Date { input, purpose } = &app.mode else {
        return;
    };
    let title = match purpose {
        DatePurpose::Go => "Перейти к дате".to_string(),
        DatePurpose::Postpone(index) => {
            let text = app.board.list(app.active).get(*index).map_or("", |t| t.text.as_str());
            format!("Перенести «{text}» на дату")
        }
    };
    let preview = if input.text().trim().is_empty() {
        " 2026-10-27 · 27.10 · +1 · -3 ".fg(MUTED)
    } else {
        match app.input_date(input) {
            Some(date) => format!(" {} ", page_date(date)).fg(ACCENT),
            None => " не дата ".red(),
        }
    };
    let block = panel(title).title_bottom(Line::from(preview).centered());
    draw_field(frame, block, input, hits);
}

/// Окно с однострочным полем ввода.
fn draw_field(frame: &mut Frame, block: Block<'static>, input: &LineInput, hits: &mut Hits) {
    let area = popup_area(frame.area(), 70, 3);
    hits.push((area, Hit::Block));
    let inner = block.inner(area);

    // Горизонтальная прокрутка, чтобы курсор оставался в поле.
    let width = usize::from(inner.width.max(1));
    let cursor = input.cursor_width();
    let offset = cursor.saturating_sub(width - 1);

    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(input.text())
            .scroll((0, offset as u16))
            .block(block),
        area,
    );
    frame.set_cursor_position((inner.x + (cursor - offset) as u16, inner.y));
}

fn draw_confirm(frame: &mut Frame, app: &App, hits: &mut Hits) {
    let Some(task) = app.selected_task() else {
        return;
    };
    let note = notes_mark(app, task).map(|_| {
        format!("Записи задачи сохранятся в {}", crate::book::DELETED_NOTES)
    });
    draw_question(frame, "Удаление", &format!("Удалить «{}»?", task.text), note.as_deref(), hits);
}

/// Окно «y — да» с вопросом и необязательным пояснением.
fn draw_question(frame: &mut Frame, title: &str, question: &str, note: Option<&str>, hits: &mut Hits) {
    let answers = Line::from(vec![
        Span::raw("y").red().bold(),
        Span::raw(" — да, любая другая клавиша — нет").fg(MUTED),
    ]);
    draw_question_with(frame, title, question, note, answers, hits);
}

/// То же со своими вариантами ответа.
fn draw_question_with(
    frame: &mut Frame,
    title: &str,
    question: &str,
    note: Option<&str>,
    answers: Line<'static>,
    hits: &mut Hits,
) {
    let area = popup_area(frame.area(), 60, if note.is_some() { 6 } else { 5 });
    hits.push((area, Hit::Block));
    let block = panel(title).border_style(Style::new().red());
    let mut text = vec![Line::from(question.to_string())];
    if let Some(note) = note {
        text.push(Line::from(note.to_string().fg(SUB)));
    }
    text.push(answers);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: true }).block(block),
        area,
    );
}

/// Справка по клавишам; в окне записей — по его клавишам.
fn draw_help(frame: &mut Frame, notes: bool) {
    let render = |sections: &[Section]| {
        let mut lines = vec![];
        for &section in sections {
            lines.push(Line::from(section.title().to_uppercase().fg(SUB).bold()));
            let rows = keymap::ROWS.iter().chain(keymap::NOTES_ROWS);
            for row in rows.filter(|r| r.section == section) {
                let Some(help) = row.help else { continue };
                lines.push(Line::from(vec![
                    Span::raw(format!("   {:<16}", keymap::row_keys(row))).fg(ACCENT).bold(),
                    Span::raw(help),
                ]));
            }
            lines.push(Line::from(""));
        }
        lines.pop();
        lines
    };
    const COL: u16 = 46;
    let area = frame.area();
    // Две колонки, если помещаются, иначе одна (на очень маленьком терминале обрезается снизу).
    let columns = if notes {
        vec![render(&[Section::Notes, Section::Field])]
    } else if area.width >= 2 * COL + 6 {
        vec![
            render(&[Section::Tasks, Section::Field]),
            render(&[Section::Pages, Section::Navigation]),
        ]
    } else {
        vec![render(&Section::ALL)]
    };
    let rows = columns.iter().map(Vec::len).max().unwrap_or(0) as u16;
    let width = (columns.len() as u16 * COL + 4).min(area.width);
    let r = centered(area, width, rows + 4);
    frame.render_widget(Clear, r);
    let block = panel("Горячие клавиши")
        .title_bottom(any_key_closes())
        .padding(Padding::uniform(1));
    let inner = block.inner(r);
    frame.render_widget(block, r);
    let cols = Layout::horizontal(vec![Constraint::Fill(1); columns.len()]).split(inner);
    for (lines, col) in columns.into_iter().zip(cols.iter()) {
        frame.render_widget(Paragraph::new(lines), *col);
    }
}

fn draw_about(frame: &mut Frame) -> Vec<Link> {
    // По строке на запись: (текст, стиль, ссылка); `None` — пустая строка.
    let rows: [Option<(String, Style, Option<&'static str>)>; 6] = [
        Some(("ztaskmgr".into(), Style::new().fg(ACCENT).bold(), None)),
        None,
        Some((env!("CARGO_PKG_DESCRIPTION").into(), Style::new(), None)),
        Some((format!("Версия {}", env!("CARGO_PKG_VERSION")), Style::new().fg(SUB), None)),
        None,
        Some((REPO_URL.into(), Style::new().fg(ACCENT).underlined(), Some(REPO_URL))),
    ];
    let row_width = |row: &Option<(String, Style, Option<&'static str>)>| {
        row.as_ref().map_or(0, |(t, ..)| Span::raw(t.as_str()).width())
    };
    let widest = rows.iter().map(row_width).max().unwrap_or(0) as u16;
    let r = centered(frame.area(), widest + 10, rows.len() as u16 + 4);
    frame.render_widget(Clear, r);
    let block = panel("О программе")
        .title_bottom(any_key_closes())
        .padding(Padding::uniform(1));
    let inner = block.inner(r);
    frame.render_widget(block, r);
    let buf = frame.buffer_mut();
    let mut links = Vec::new();
    for (n, row) in rows.iter().enumerate().take(usize::from(inner.height)) {
        let Some((text, style, url)) = row else { continue };
        let y = inner.y + n as u16;
        let w = row_width(row) as u16;
        if w > inner.width {
            // Для ссылки слишком узко: простой обрезанный текст.
            buf.set_stringn(inner.x, y, text, usize::from(inner.width), Style::new().fg(SUB));
            continue;
        }
        let x = inner.x + (inner.width - w) / 2;
        buf.set_string(x, y, text, *style);
        if let Some(url) = url {
            links.push(Link {
                x,
                y,
                text: text.clone(),
                url,
            });
        }
    }
    links
}

/// Прямоугольник по центру `area`, не больше её самой.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    area
}

/// Окно ввода по центру экрана, не шире `max_width` и с отступом от краёв.
fn popup_area(area: Rect, max_width: u16, height: u16) -> Rect {
    centered(area, max_width.min(area.width.saturating_sub(4)).max(10), height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Book;
    use crate::input::{Mouse, MouseKind};
    use crate::model::Task;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(terminal: &mut Terminal<TestBackend>, app: &mut App) -> Vec<String> {
        terminal.draw(|f| {
            draw(f, app);
        }).unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect())
            .collect()
    }

    fn tap(app: &mut App, hit: Hit) {
        let (rect, _) = *app.hits.iter().rev().find(|(_, h)| *h == hit).unwrap_or_else(|| panic!("нет {hit:?}"));
        app.on_mouse(Mouse { kind: MouseKind::Click, column: rect.x, row: rect.y });
    }

    fn phone_app(name: &str) -> App {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-ui-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut app = App::new(Book::new(dir), crate::config::Config::default(), Date::today());
        for text in ["раз", "два"] {
            app.board.push(ListId::WORK, Task::new(text));
        }
        app.lists.entry(ListId::WORK).or_default().select(Some(0));
        app
    }

    #[test]
    fn many_lists() {
        use ratatui::crossterm::event::KeyEvent;
        let dir = std::env::temp_dir().join(format!("ztaskmgr-ui-{}-many", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let book = Book::new(dir);
        let mut lists = book.lists().unwrap();
        for n in 5..=25 {
            lists.add(ListId::new(&format!("x{n}")), &format!("Список {n}")).unwrap();
        }
        book.save_lists(&lists).unwrap();
        let mut app = App::new(book, crate::config::Config::default(), Date::today());
        app.board.push(ListId::new("x25"), Task::new("последняя"));
        let has = |screen: &[String], text: &str| screen.iter().any(|l| l.contains(text));

        // 25 списков — сетка 5×5, номера у первых девяти.
        let mut wide = Terminal::new(TestBackend::new(150, 40)).unwrap();
        let screen = render(&mut wide, &mut app);
        assert!(has(&screen, "╭ 9 Список 9") && has(&screen, "╭ Список 10") && has(&screen, "последняя"), "{screen:#?}");
        let tops = screen.iter().filter(|l| l.contains("╭ 1 Работа") || l.contains("╭ Список 21")).count();
        assert_eq!(tops, 2, "{screen:#?}");

        // m — окно выбора из 24 списков.
        app.on_key(KeyEvent::from(KeyCode::Char('m')));
        assert!(matches!(app.mode, Mode::Normal), "пустой список — переносить нечего");
        app.active = ListId::new("x25");
        app.lists.entry(app.active).or_default().select(Some(0));
        app.on_key(KeyEvent::from(KeyCode::Char('m')));
        let screen = render(&mut wide, &mut app);
        assert!(has(&screen, "Перенести в список") && has(&screen, "1 Работа"), "{screen:#?}");
        tap(&mut app, Hit::Pick(1));
        assert_eq!(app.board.list(ListId::HOME)[0].text, "последняя");

        // Узкий экран: вкладки вокруг активной, по краям ‹ ›.
        app.active = ListId::new("x12");
        let mut phone = Terminal::new(TestBackend::new(44, 16)).unwrap();
        let screen = render(&mut phone, &mut app);
        assert!(screen[1].contains('‹') && screen[1].contains('›') && screen[1].contains("12 Список 12"), "{screen:#?}");
        tap(&mut app, Hit::List(ListId::new("x13")));
        assert_eq!(app.active, ListId::new("x13"));

        // Окно списков.
        app.on_key(KeyEvent::from(KeyCode::Char('L')));
        let mut tall = Terminal::new(TestBackend::new(100, 40)).unwrap();
        let screen = render(&mut tall, &mut app);
        assert!(has(&screen, "Списки · 25 из 25"), "{screen:#?}");
        let _ = std::fs::remove_dir_all(app.book.dir());
    }

    #[test]
    fn preview_and_settings() {
        use ratatui::crossterm::event::KeyEvent;
        let dir = std::env::temp_dir().join(format!("ztaskmgr-ui-{}-preview", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let book = Book::new(dir);
        let today = Date::today();
        // Задача переехала со вчерашней страницы, у неё подзадача и заметка в две строки.
        let mut task = Task::new("починить кран");
        task.id = Some("pv1".into());
        let mut yesterday = crate::model::Board::default();
        let mut moved = task.clone();
        moved.state = State::Migrated(today);
        yesterday.push(ListId::WORK, moved);
        book.save(today.add_days(-1), &yesterday).unwrap();
        let mut page = crate::model::Board::default();
        page.push(ListId::WORK, task);
        book.save(today, &page).unwrap();
        let mut notes = crate::notes::Notes::default();
        notes.add_subtask("купить прокладку");
        notes.add_note("27.10.2026 19:40", "мастер сказал:\nменять смеситель");
        book.save_notes("pv1", &notes).unwrap();

        let mut app = App::new(book, crate::config::Config::default(), today);
        let has = |screen: &[String], text: &str| screen.iter().any(|l| l.contains(text));
        let mut wide = Terminal::new(TestBackend::new(140, 30)).unwrap();
        let screen = render(&mut wide, &mut app);
        for text in ["Превью", "открыта", "История", "← эта страница", "подзадачи 0/1", "[ ] купить прокладку", "менять смеситель"] {
            assert!(has(&screen, text), "{text}: {screen:#?}");
        }
        // Тап по превью — окно записей.
        tap(&mut app, Hit::Preview);
        assert!(app.notes.is_some());
        app.on_key(KeyEvent::from(KeyCode::Esc));

        // На экране уже 120 колонок превью нет.
        let mut medium = Terminal::new(TestBackend::new(110, 30)).unwrap();
        assert!(!has(&render(&mut medium, &mut app), "╭ Превью"));

        // v — выключить, в настройках (S) — включить обратно, тапом по пункту.
        app.on_key(KeyEvent::from(KeyCode::Char('v')));
        assert!(!app.config.preview());
        let screen = render(&mut wide, &mut app);
        assert!(!has(&screen, "╭ Превью") && has(&screen, "Превью выключено"), "{screen:#?}");
        app.on_key(KeyEvent::from(KeyCode::Char('S')));
        let screen = render(&mut wide, &mut app);
        assert!(has(&screen, "[ ] Превью задачи справа"), "{screen:#?}");
        tap(&mut app, Hit::Setting(0));
        assert!(app.config.preview());
        app.on_key(KeyEvent::from(KeyCode::Char(' ')));
        assert!(!app.config.preview());
        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Normal));
        let _ = std::fs::remove_dir_all(app.book.dir());
    }

    #[test]
    fn phone_layout_and_taps() {
        let mut app = phone_app("phone");
        let mut terminal = Terminal::new(TestBackend::new(44, 16)).unwrap();
        let screen = render(&mut terminal, &mut app);
        // Узкий экран: вкладки и один список.
        assert!(screen[1].contains("1 Работа 0/2"), "{screen:#?}");
        assert!(screen[2].contains("1 Работа") && !screen.iter().any(|l| l.contains("╭ 2 Дом")), "{screen:#?}");
        assert!(screen[3].contains("[ ] раз"), "{screen:#?}");

        // Тап по задаче — выделить, по выделенной — записи, по подсказке «закрыть» — закрыть.
        tap(&mut app, Hit::Task(ListId::WORK, 1));
        assert_eq!(app.selected(), Some(1));
        render(&mut terminal, &mut app);
        tap(&mut app, Hit::Task(ListId::WORK, 1));
        assert!(app.notes.is_some());
        render(&mut terminal, &mut app);
        // Окно записей закрывает собой список под ним.
        let last = |f: fn(&Hit) -> bool| app.hits.iter().rposition(|(_, h)| f(h));
        assert!(last(|h| *h == Hit::Block) > last(|h| matches!(h, Hit::Task(..))));
        tap(&mut app, Hit::Key((KeyCode::Esc, KeyModifiers::NONE)));
        assert!(app.notes.is_none());

        // `[ ]` — отметить; вкладка — другой список; ▶ — следующий день.
        render(&mut terminal, &mut app);
        tap(&mut app, Hit::Check(ListId::WORK, 0));
        let work = app.board.list(ListId::WORK);
        assert_eq!((work[1].text.as_str(), work[1].state), ("раз", State::Done));
        render(&mut terminal, &mut app);
        tap(&mut app, Hit::List(ListId::HOME));
        assert_eq!(app.active, ListId::HOME);
        render(&mut terminal, &mut app);
        let today = app.date;
        tap(&mut app, Hit::Key((KeyCode::Char(']'), KeyModifiers::NONE)));
        assert_eq!(app.date, today.add_days(1));
        // Подсказка «добавить» — как нажать `a`.
        render(&mut terminal, &mut app);
        tap(&mut app, Hit::Key((KeyCode::Char('a'), KeyModifiers::NONE)));
        assert!(matches!(app.mode, Mode::Edit { .. }));
        let _ = std::fs::remove_dir_all(app.book.dir());
    }

    #[test]
    fn wide_layout_scroll_and_popup_block() {
        let mut app = phone_app("wide");
        app.board.push(ListId::HOME, Task::new("кран"));
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let screen = render(&mut terminal, &mut app);
        assert!(screen.iter().any(|l| l.contains("1 Работа") && l.contains("2 Дом")), "{screen:#?}");
        // Свайп (прокрутка) над «Дом» — туда и выделение.
        let (rect, _) = *app.hits.iter().find(|(_, h)| *h == Hit::List(ListId::HOME)).unwrap();
        app.on_mouse(Mouse { kind: MouseKind::ScrollDown, column: rect.x + 3, row: rect.y + 3 });
        assert_eq!(app.active, ListId::HOME);
        // Под окном ввода список не тапается.
        app.on_key(KeyCode::Char('a'));
        render(&mut terminal, &mut app);
        let (rect, _) = *app.hits.iter().rev().find(|(_, h)| *h == Hit::Block).unwrap();
        app.on_mouse(Mouse { kind: MouseKind::Click, column: rect.x + 1, row: rect.y + 1 });
        assert!(matches!(app.mode, Mode::Edit { .. }));
        let _ = std::fs::remove_dir_all(app.book.dir());
    }

    #[test]
    fn truncate_path() {
        assert_eq!(truncate_start("~/tasks.md", 20), "~/tasks.md");
        assert_eq!(truncate_start("~/долгий/путь/tasks.md", 10), "…/tasks.md");
        assert_eq!(truncate_start("abc", 0), "…");
    }

    #[test]
    fn home_as_tilde() {
        let home = BaseDirs::new().unwrap().home_dir().to_path_buf();
        assert_eq!(display_path(&home.join("a/tasks.md")), "~/a/tasks.md");
        assert_eq!(display_path(Path::new("/tmp/t.md")), "/tmp/t.md");
    }
}
