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

use crate::app::{App, AUTOSAVE, DatePurpose, Draft, Mode, NoteTarget, NotesMode, NotesView, Status};
use crate::clock::{self, Date};
use crate::field::LineInput;
use crate::keymap::{self, Action as A, Section};
use crate::model::{Category, State, Task};
use crate::field::TextArea;
use crate::notes::Kind;

const ACCENT: Color = Color::Cyan;
const SUB: Color = Color::Gray;
const MUTED: Color = Color::DarkGray;

pub const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// Кликабельная ссылка (OSC 8) поверх уже нарисованного кадра, см. `write_links`.
pub struct Link {
    x: u16,
    y: u16,
    text: String,
    url: &'static str,
}

/// Рисует кадр; возвращает ссылки, которые надо дописать поверх него (`write_links`).
pub fn draw(frame: &mut Frame, app: &mut App) -> Vec<Link> {
    let [header_area, board_area, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    draw_header(frame, app, header_area);
    let rows: [Rect; 2] = Layout::vertical([Constraint::Ratio(1, 2); 2]).areas(board_area);

    // Раскладка 2×2: Работа | Дом / Личное | Прочее.
    for category in Category::ALL {
        let i = category.index();
        let cols: [Rect; 2] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).areas(rows[i / 2]);
        draw_list(frame, app, category, cols[i % 2]);
    }

    draw_status(frame, app, status_area);

    match &app.mode {
        Mode::Edit { .. } => draw_edit(frame, app),
        Mode::Date { .. } => draw_date(frame, app),
        Mode::ConfirmDelete => draw_confirm(frame, app),
        Mode::Normal | Mode::MoveTo => {}
    }
    if app.notes.is_some() {
        draw_notes(frame, app);
    }
    if app.show_help {
        draw_help(frame, app.notes.is_some());
    }
    if app.show_about {
        return draw_about(frame);
    }
    Vec::new()
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

/// Шапка: название, дата открытой страницы и каталог книжки слева, текущие дата и время справа.
fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let right = Line::from(format!("{} ", clock::now()).fg(SUB));
    let relative = match app.today.days_until(app.date) {
        0 => " · сегодня",
        -1 => " · вчера",
        1 => " · завтра",
        _ => "",
    };
    let title = [
        Span::raw(" ≡ ").fg(Color::Black).bg(ACCENT).bold(),
        Span::raw(" ztaskmgr ").fg(ACCENT).bold(),
        Span::raw("  "),
        Span::raw(page_date(app.date)).bold(),
        Span::raw(relative).fg(MUTED),
        Span::raw("   "),
    ];
    let title_w: usize = title.iter().map(Span::width).sum();
    // Между путём и часами — хотя бы два пробела; на узком экране путь обрезается слева.
    let room = usize::from(area.width).saturating_sub(title_w + right.width() + 2);
    let path = truncate_start(&display_path(app.book.dir()), room);
    let mut left = title.to_vec();
    left.push(Span::raw(path).fg(SUB));
    frame.render_widget(Line::from(left), area);
    frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
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

fn draw_list(frame: &mut Frame, app: &mut App, category: Category, area: Rect) {
    let active = app.active == category;
    let tasks = app.board.list(category);
    // Перенесённые — уже не дела этой страницы, в счётчике их нет.
    let done = tasks.iter().filter(|t| t.state == State::Done).count();
    let total = tasks
        .iter()
        .filter(|t| !matches!(t.state, State::Migrated(_)))
        .count();

    let border_style = if active {
        Style::new().fg(ACCENT).bold()
    } else {
        Style::new().fg(MUTED)
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(format!(" {} {} ", category.index() + 1, category.title()))
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
                State::Migrated(to) => Line::from(vec![
                    Span::raw("[>] ").fg(MUTED),
                    Span::raw(task.text.as_str()).fg(MUTED),
                    Span::raw(format!(" → {}", short_date(to, app.date))).fg(MUTED).italic(),
                ]),
            };
            if let Some(mark) = notes_mark(app, task) {
                line.push_span(Span::raw(mark).fg(SUB));
            }
            line.into()
        })
        .collect();

    let (symbol, highlight) = if active {
        ("› ", Style::new().reversed())
    } else {
        ("  ", Style::new())
    };
    let list = List::new(items)
        .block(block)
        .highlight_symbol(symbol)
        .highlight_style(highlight)
        .highlight_spacing(HighlightSpacing::Always);

    frame.render_stateful_widget(list, area, &mut app.lists[category.index()]);
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

fn key_hint(key: &str, action: &str) -> [Span<'static>; 2] {
    [
        Span::raw(key.to_string()).fg(ACCENT).bold(),
        Span::raw(format!(" {action}   ")).fg(MUTED),
    ]
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    match &app.status {
        Some(Status::Error(text)) => {
            frame.render_widget(Line::from(format!(" {text}").red()), area);
            return;
        }
        Some(Status::Info(text)) => {
            frame.render_widget(Line::from(format!(" {text}").fg(ACCENT)), area);
            return;
        }
        None => {}
    }
    if let Some(view) = &app.notes {
        draw_notes_status(frame, view, area);
        return;
    }
    let hints: Vec<(String, &str)> = match app.mode {
        Mode::Normal => {
            let actions: &[(&[A], &str)] = &[
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
            ];
            actions
                .iter()
                .map(|(a, d)| (keymap::hint_keys(a), *d))
                .collect()
        }
        Mode::Edit { .. } => vec![("Enter".into(), "сохранить"), ("Esc".into(), "отмена")],
        Mode::Date { purpose, .. } => {
            let enter = match purpose {
                DatePurpose::Go => "перейти",
                DatePurpose::Postpone(_) => "перенести",
            };
            vec![("Enter".into(), enter), ("Esc".into(), "отмена")]
        }
        Mode::ConfirmDelete => vec![("y/Enter".into(), "удалить"), ("другая".into(), "отмена")],
        Mode::MoveTo => {
            frame.render_widget(move_to_hint(app.active), area);
            return;
        }
    };
    let main = matches!(app.mode, Mode::Normal);
    draw_hint_line(frame, area, &hints, &keymap::hint_keys(&[A::Help]), main);
}

fn draw_notes_status(frame: &mut Frame, view: &NotesView, area: Rect) {
    let hints: Vec<(String, &str)> = match view.mode {
        NotesMode::List => {
            let actions: &[(&[A], &str)] = &[
                (&[A::Add], "заметка"),
                (&[A::AddSubtask], "подзадача"),
                (&[A::Edit], "правка"),
                (&[A::Toggle], "готово"),
                (&[A::MoveUp, A::MoveDown], "сдвиг"),
                (&[A::Delete], "удалить"),
                (&[A::OpenEditor], "редактор"),
                (&[A::Help], "помощь"),
                (&[A::Close], "закрыть"),
            ];
            actions
                .iter()
                .map(|(a, d)| (keymap::hint_keys_in(keymap::NOTES_ROWS, a), *d))
                .collect()
        }
        NotesMode::Edit { .. } => vec![("Enter".into(), "сохранить"), ("Esc".into(), "отмена")],
        NotesMode::Text(Draft { confirm_close: true, .. }) => vec![
            ("y/Enter".into(), "закрыть"),
            ("s/Ctrl+S".into(), "сохранить и закрыть"),
            ("другая".into(), "вернуться"),
        ],
        NotesMode::Text(_) => vec![
            ("Ctrl+S".into(), "сохранить"),
            ("Enter".into(), "новая строка"),
            ("Esc".into(), "закрыть"),
        ],
        NotesMode::ConfirmDelete => vec![("y/Enter".into(), "удалить"), ("другая".into(), "отмена")],
    };
    let help = keymap::hint_keys_in(keymap::NOTES_ROWS, &[A::Help]);
    draw_hint_line(frame, area, &hints, &help, false);
}

/// Окно доп. записей задачи: подзадачи чекбоксами, заметки с датой, прочие строки как есть.
fn draw_notes(frame: &mut Frame, app: &mut App) {
    let Some(title) = app.notes_task().map(|t| format!("Записи · {}", t.text)) else {
        return;
    };
    let Some(view) = &mut app.notes else {
        return;
    };
    let screen = frame.area();
    let width = (screen.width * 4 / 5).clamp(40, 100);
    let height = (screen.height * 3 / 4).max(8);
    let area = centered(screen, width, height);
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

    let items: Vec<ListItem> = visible
        .iter()
        .map(|&i| {
            let item = view.notes.item(i);
            let (prefix, style): (Vec<Span>, Style) = match item.kind {
                Kind::Subtask { done: false } => (vec![Span::raw("[ ] ")], Style::new()),
                Kind::Subtask { done: true } => (
                    vec![Span::raw("[x] ").fg(MUTED)],
                    Style::new().fg(MUTED).crossed_out(),
                ),
                Kind::Note { stamp } => (vec![Span::raw(format!("{stamp}  ")).fg(MUTED)], Style::new()),
                Kind::Text => (vec![], Style::new()),
            };
            // Строки пункта и длинные строки (по словам) — с отступом под префикс.
            let indent: usize = prefix.iter().map(Span::width).sum();
            let wrap_width = text_width.saturating_sub(indent).max(5);
            let chunks = std::iter::once(item.head)
                .chain(item.body.iter().copied())
                .flat_map(|line| wrap(line, wrap_width));
            let lines: Vec<Line> = chunks
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
                .collect();
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

    match &mut view.mode {
        NotesMode::List => {}
        NotesMode::Edit { input, target } => {
            let title = match target {
                NoteTarget::Entry(_) => "Правка подзадачи",
                _ => "Новая подзадача",
            };
            draw_field(frame, panel(title), input);
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
            draw_text_area(frame, title, &saved, &mut draft.area);
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
                );
            }
        }
        NotesMode::ConfirmDelete => {
            let text = view.selected_entry().map(|i| view.notes.editable_text(i)).unwrap_or_default();
            // В вопросе — первая строка пункта.
            let first = text.lines().next().unwrap_or("");
            draw_question(frame, "Удаление", &format!("Удалить «{first}»?"), None);
        }
    }
}

/// Многострочное поле: высота по тексту (от 3 до 12 строк), прокрутка за курсором.
fn draw_text_area(frame: &mut Frame, title: &str, saved: &str, area: &mut TextArea) {
    let screen = frame.area();
    let width = 70.min(screen.width.saturating_sub(4)).max(20);
    let probe = panel("").inner(Rect::new(0, 0, width, 3));
    area.set_width(usize::from(probe.width));
    let rows = area.rows();
    let max_rows = 12.min(screen.height.saturating_sub(6)).max(3);
    let height = (rows.len() as u16).clamp(3, max_rows);
    let rect = centered(screen, width, height + 2);
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

/// Подсказки, обрезанные с конца по ширине (подсказка `keep`, справка, остаётся),
/// и «о программе» у правого края на главном экране.
fn draw_hint_line(frame: &mut Frame, area: Rect, hints: &[(String, &str)], keep: &str, main: bool) {
    let about = main.then(|| {
        Line::from(vec![
            Span::raw(keymap::hint_keys(&[A::About])).fg(ACCENT).bold(),
            Span::raw(" о программе ").fg(MUTED),
        ])
    });
    let right_w = about.as_ref().map_or(0, Line::width);
    let hint_w = |(k, d): &&(String, &str)| Line::from(key_hint(k, d).to_vec()).width();
    let mut shown: Vec<&(String, &str)> = hints.iter().collect();
    while 1 + shown.iter().map(hint_w).sum::<usize>() + right_w > usize::from(area.width) {
        let Some(last) = shown.iter().rposition(|(k, _)| k != keep) else {
            break;
        };
        shown.remove(last);
    }
    let mut spans = vec![Span::raw(" ")];
    for (k, d) in shown {
        spans.extend(key_hint(k, d));
    }
    frame.render_widget(Line::from(spans), area);
    if let Some(about) = about {
        frame.render_widget(Paragraph::new(about).alignment(Alignment::Right), area);
    }
}

fn move_to_hint(from: Category) -> Line<'static> {
    let mut spans = vec![Span::raw(" Перенести в: ")];
    for category in Category::ALL {
        let key = Span::raw(format!("{} ", category.index() + 1));
        let title = Span::raw(format!("{}  ", category.title()));
        if category == from {
            spans.push(key.fg(MUTED));
            spans.push(title.fg(MUTED));
        } else {
            spans.push(key.fg(ACCENT).bold());
            spans.push(title);
        }
    }
    spans.push(Span::raw("(другая клавиша — отмена)").fg(MUTED));
    Line::from(spans)
}

fn draw_edit(frame: &mut Frame, app: &App) {
    let Mode::Edit { input, index } = &app.mode else {
        return;
    };
    let action = if index.is_some() { "Правка" } else { "Новая задача" };
    draw_field(frame, panel(format!("{action} · {}", app.active.title())), input);
}

/// Окно даты; внизу — во что разбирается введённое.
fn draw_date(frame: &mut Frame, app: &App) {
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
    draw_field(frame, block, input);
}

/// Окно с однострочным полем ввода.
fn draw_field(frame: &mut Frame, block: Block<'static>, input: &LineInput) {
    let area = popup_area(frame.area(), 70, 3);
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

fn draw_confirm(frame: &mut Frame, app: &App) {
    let Some(task) = app.selected_task() else {
        return;
    };
    let note = notes_mark(app, task).map(|_| {
        format!("Записи задачи сохранятся в {}", crate::book::DELETED_NOTES)
    });
    draw_question(frame, "Удаление", &format!("Удалить «{}»?", task.text), note.as_deref());
}

/// Окно «y — да» с вопросом и необязательным пояснением.
fn draw_question(frame: &mut Frame, title: &str, question: &str, note: Option<&str>) {
    let answers = Line::from(vec![
        Span::raw("y").red().bold(),
        Span::raw(" — да, любая другая клавиша — нет").fg(MUTED),
    ]);
    draw_question_with(frame, title, question, note, answers);
}

/// То же со своими вариантами ответа.
fn draw_question_with(
    frame: &mut Frame,
    title: &str,
    question: &str,
    note: Option<&str>,
    answers: Line<'static>,
) {
    let area = popup_area(frame.area(), 60, if note.is_some() { 6 } else { 5 });
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
