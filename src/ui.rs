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

use crate::app::{App, Mode};
use crate::clock;
use crate::keymap::{self, Action as A, Section};
use crate::model::Category;

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
        Mode::ConfirmDelete => draw_confirm(frame, app),
        Mode::Normal | Mode::MoveTo => {}
    }
    if app.show_help {
        draw_help(frame);
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

/// Шапка: название, открытый файл слева, дата и время справа.
fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let right = Line::from(format!("{} ", clock::now()).fg(SUB));
    let title = [
        Span::raw(" ≡ ").fg(Color::Black).bg(ACCENT).bold(),
        Span::raw(" ztaskmgr ").fg(ACCENT).bold(),
        Span::raw("  "),
    ];
    let title_w: usize = title.iter().map(Span::width).sum();
    // Между путём и часами — хотя бы два пробела; на узком экране путь обрезается слева.
    let room = usize::from(area.width).saturating_sub(title_w + right.width() + 2);
    let path = truncate_start(&display_path(&app.path), room);
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
    let done = tasks.iter().filter(|t| t.done).count();

    let border_style = if active {
        Style::new().fg(ACCENT).bold()
    } else {
        Style::new().fg(MUTED)
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(format!(" {} {} ", category.index() + 1, category.title()))
        .title(Line::from(format!(" {done}/{} ", tasks.len())).right_aligned());

    if tasks.is_empty() {
        let hint = Paragraph::new("пусто".fg(MUTED)).centered().block(block);
        frame.render_widget(hint, area);
        return;
    }

    let items: Vec<ListItem> = tasks
        .iter()
        .map(|task| {
            if task.done {
                Line::from(vec![
                    Span::raw("[x] ").fg(MUTED),
                    Span::raw(task.text.as_str()).fg(MUTED).crossed_out(),
                ])
            } else {
                Line::raw(format!("[ ] {}", task.text))
            }
            .into()
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

fn key_hint(key: &str, action: &str) -> [Span<'static>; 2] {
    [
        Span::raw(key.to_string()).fg(ACCENT).bold(),
        Span::raw(format!(" {action}   ")).fg(MUTED),
    ]
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(status) = &app.status {
        frame.render_widget(Line::from(format!(" {status}").red()), area);
        return;
    }
    let hints: Vec<(String, &str)> = match app.mode {
        Mode::Normal => {
            let actions: &[(&[A], &str)] = &[
                (&[A::Add], "добавить"),
                (&[A::Edit], "правка"),
                (&[A::Toggle], "готово"),
                (&[A::MoveUp, A::MoveDown], "сдвиг"),
                (&[A::MoveTo], "перенос"),
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
        Mode::ConfirmDelete => vec![("y/Enter".into(), "удалить"), ("другая".into(), "отмена")],
        Mode::MoveTo => {
            frame.render_widget(move_to_hint(app.active), area);
            return;
        }
    };
    let main = matches!(app.mode, Mode::Normal);
    draw_hint_line(frame, area, &hints, &keymap::hint_keys(&[A::Help]), main);
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
    let area = popup_area(frame.area(), 70, 3);
    let block = panel(format!("{action} · {}", app.active.title()));
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
    let area = popup_area(frame.area(), 60, 5);
    let block = panel("Удаление").border_style(Style::new().red());
    let text = vec![
        Line::from(format!("Удалить «{}»?", task.text)),
        Line::from(vec![
            Span::raw("y").red().bold(),
            Span::raw(" — да, любая другая клавиша — нет").fg(MUTED),
        ]),
    ];
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: true }).block(block),
        area,
    );
}

fn draw_help(frame: &mut Frame) {
    let render = |sections: &[Section]| {
        let mut lines = vec![];
        for &section in sections {
            lines.push(Line::from(section.title().to_uppercase().fg(SUB).bold()));
            for row in keymap::ROWS.iter().filter(|r| r.section == section) {
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
    let columns = if area.width >= 2 * COL + 6 {
        vec![
            render(&[Section::Tasks, Section::Field]),
            render(&[Section::Navigation]),
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
