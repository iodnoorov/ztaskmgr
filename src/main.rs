mod app;
mod book;
mod clock;
mod field;
mod input;
mod keymap;
mod model;
mod storage;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::{execute, terminal};

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
