mod app;
mod book;
mod clock;
mod field;
mod input;
mod keymap;
mod model;
mod notes;
mod storage;
mod ui;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::crossterm::{cursor, execute, terminal};

use crate::app::{App, Status};
use crate::book::Book;
use crate::clock::Date;
use crate::input::Input;

const USAGE: &str = "\
Использование: ztaskmgr [КАТАЛОГ]

Записная книжка дел: страница на каждую дату, на странице четыре списка —
Работа, Дом, Личное, Прочее.
КАТАЛОГ — где лежат страницы (ГГГГ-ММ-ДД.md), по умолчанию ~/.local/share/ztaskmgr.";

/// Как часто проверять размер терминала, часы и смену дня, пока ввода нет.
const RESIZE_POLL: Duration = Duration::from_millis(100);

fn main() -> Result<()> {
    clock::init();
    let Some(dir) = parse_args()? else {
        println!("{USAGE}");
        return Ok(());
    };
    if dir.is_file() {
        bail!("{} — файл, а нужен каталог книжки\n\n{USAGE}", dir.display());
    }
    let book = Book::new(dir);
    let today = Date::today();
    // Автоперенос и страницу на сегодня — до перехода в TUI, чтобы ошибка разбора
    // вывелась в обычный терминал.
    let carried = book.carry_over(today)?;
    book.load(today)?;
    let mut app = App::new(book, today);
    if let Some((from, count)) = carried {
        app.status = Some(Status::Info(app::carried_message(from, count)));
    }

    let mut terminal = ratatui::init();
    // Вставленный текст приходит одним событием, а не потоком горячих клавиш.
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let mut kitty = false;
    let result = run(&mut terminal, &mut app, &mut kitty);
    if kitty {
        input::kitty_off();
    }
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal, app: &mut App, kitty: &mut bool) -> Result<()> {
    // Клавиши читаем сами, а не через crossterm: он теряет физическую клавишу (см. `input`).
    let mut reader = input::Reader::new();
    let mut size = terminal::size()?;
    let mut time = clock::now();
    let mut dirty = true;
    while !app.quit {
        if dirty {
            let mut links = Vec::new();
            terminal.draw(|frame| links = ui::draw(frame, app))?;
            ui::write_links(&links)?;
            dirty = false;
        }
        for event in reader.read(RESIZE_POLL)? {
            match event {
                Input::KittyProtocol => {
                    if !std::mem::replace(kitty, true) {
                        input::kitty_on();
                    }
                }
                Input::Key(press) => app.on_key(press),
                Input::Paste(text) => app.on_paste(&text),
            }
            dirty = true;
        }
        if app.tick(Instant::now()) {
            dirty = true;
        }
        if let Some(path) = app.editor_request.take() {
            let error = run_editor(terminal, &path, *kitty)?;
            app.after_editor(error);
            dirty = true;
        }
        let now = terminal::size()?;
        if now != size {
            size = now;
            dirty = true;
        }
        let now = clock::now();
        if now != time {
            time = now;
            dirty = true;
            let today = Date::today();
            if today != app.today {
                app.on_new_day(today);
            }
        }
    }
    Ok(())
}

/// Внешний редактор (`$VISUAL`, `$EDITOR`, иначе `vi`): терминал на это время отдаётся ему.
/// Возвращает ошибку запуска редактора для строки состояния.
fn run_editor(terminal: &mut DefaultTerminal, path: &Path, kitty: bool) -> Result<Option<String>> {
    let editor = ["VISUAL", "EDITOR"]
        .into_iter()
        .filter_map(|var| std::env::var(var).ok())
        .find(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "vi".into());
    // Значение может быть с аргументами: `code -w`.
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");

    if kitty {
        input::kitty_off();
    }
    execute!(std::io::stdout(), DisableBracketedPaste, LeaveAlternateScreen, cursor::Show)?;
    terminal::disable_raw_mode()?;
    let status = Command::new(program).args(parts).arg(path).status();
    terminal::enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    if kitty {
        input::kitty_on();
    }
    terminal.clear()?;

    Ok(match status {
        Ok(s) if s.success() => None,
        Ok(s) => Some(format!("Редактор «{editor}» завершился с ошибкой: {s}")),
        Err(e) => Some(format!("Редактор «{editor}» не запустился: {e}")),
    })
}

/// `None` — запрошена справка.
fn parse_args() -> Result<Option<PathBuf>> {
    let mut args = std::env::args_os().skip(1);
    match (args.next(), args.next()) {
        (None, _) => default_path().map(Some),
        (Some(arg), None) if arg == "-h" || arg == "--help" => Ok(None),
        (Some(arg), None) => Ok(Some(PathBuf::from(arg))),
        _ => bail!("слишком много аргументов\n\n{USAGE}"),
    }
}

fn default_path() -> Result<PathBuf> {
    ProjectDirs::from("", "", "ztaskmgr")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .context("не удалось определить домашний каталог")
}
