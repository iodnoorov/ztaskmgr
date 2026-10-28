mod agent;
mod app;
mod book;
mod clock;
mod config;
mod field;
mod input;
mod keymap;
mod lists;
mod mcp;
mod merge;
mod model;
mod notes;
#[cfg(test)]
mod proptests;
mod storage;
mod store;
mod ui;
mod webdav;

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
use crate::config::Config;
use crate::input::Input;

const USAGE: &str = "\
Использование: ztaskmgr [КАТАЛОГ]
              ztaskmgr mcp [КАТАЛОГ]
              ztaskmgr sync [КАТАЛОГ]

Записная книжка дел: страница на каждую дату, на странице списки дел — сначала
Работа, Дом, Личное, Прочее, их можно менять и добавлять свои (L).
КАТАЛОГ — где лежат страницы (ГГГГ-ММ-ДД.md), по умолчанию $ZTASKMGR_DIR,
иначе ~/.local/share/ztaskmgr. Синхронизация — свой WebDAV-сервер: адрес и вход —
в настройках (S), ~/.config/ztaskmgr/config.

sync — синхронизировать с WebDAV-сервером и выйти (например, из cron).

mcp — MCP-сервер для ИИ-агентов (stdio): книжка как инструменты, без скрытого от ИИ.
  claude mcp add ztaskmgr -- ztaskmgr mcp";

/// Как часто проверять размер терминала, часы и смену дня, пока ввода нет.
const RESIZE_POLL: Duration = Duration::from_millis(100);

fn main() -> Result<()> {
    clock::init();
    let Some(run_as) = parse_args()? else {
        println!("{USAGE}");
        return Ok(());
    };
    let (Run::Tui(dir) | Run::Mcp(dir) | Run::Sync(dir)) = &run_as;
    if dir.is_file() {
        bail!("{} — файл, а нужен каталог книжки\n\n{USAGE}", dir.display());
    }
    let book = Book::new(dir.clone());
    // Книжка прежних версий — на нынешний формат, до всякой работы с ней.
    for done in book.migrate()? {
        eprintln!("ztaskmgr: книжка обновлена: {done}");
    }
    let config = match Config::default_path() {
        Some(path) => Config::load(path)?,
        None => Config::default(),
    };
    match run_as {
        Run::Mcp(_) => {
            // Пароль из команды — один раз при запуске; не получился — без синхронизации.
            let webdav = config.webdav().map(webdav::Settings::resolve).transpose().unwrap_or_else(|e| {
                eprintln!("ztaskmgr: WebDAV: {e:#}");
                None
            });
            return mcp::serve(&agent::Agent::new(book), webdav);
        }
        Run::Sync(_) => return sync_once(&book, &config),
        Run::Tui(_) => {}
    }
    let today = Date::today();
    // Автоперенос и страница на сегодня — до перехода в TUI, чтобы ошибки вывелись в обычный
    // терминал. Синхронизация с WebDAV — уже в фоне; что она принесёт, перенесётся следом
    // (`App::on_sync`).
    let carried = book.carry_over(today)?;
    book.load(today)?;
    // Пароль из команды — сейчас, пока терминал ещё обычный.
    let (webdav, webdav_error) = match config.webdav().map(webdav::Settings::resolve).transpose() {
        Ok(settings) => (settings, None),
        Err(e) => (None, Some(format!("WebDAV: {e:#}"))),
    };
    let worker = webdav::Worker::start(dir, webdav);
    let mut app = App::new(book, config, today);
    if let Some(e) = webdav_error {
        app.status = Some(Status::Error(e));
    }
    if let Some((from, count)) = carried {
        app.status = Some(Status::Info(app::carried_message(from, count)));
    }

    let mut terminal = ratatui::init();
    // Вставленный текст приходит одним событием, а не потоком горячих клавиш.
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let mut kitty = false;
    let result = run(&mut terminal, &mut app, &mut kitty, &worker);
    if kitty {
        input::kitty_off();
    }
    input::mouse_off();
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    // Последние правки — на сервер, но не дольше нескольких секунд, если сети нет.
    if app.config.webdav().is_some() {
        println!("WebDAV: синхронизация перед выходом…");
    }
    worker.stop(Duration::from_secs(10));
    result
}

/// `ztaskmgr sync`: одна синхронизация с WebDAV и итог в терминал.
fn sync_once(book: &Book, config: &Config) -> Result<()> {
    let settings = config
        .webdav()
        .context("WebDAV не настроен: адрес — в настройках (S) или webdav_url в ~/.config/ztaskmgr/config")?
        .resolve()?;
    let summary = webdav::sync(book, &settings)?;
    match &summary {
        Some(s) => println!(
            "Отправлено: {}, получено: {}, слито: {}, удалено: {}{}",
            s.uploaded,
            s.downloaded,
            s.merged,
            s.deleted,
            if s.postponed > 0 {
                format!(", отложено до следующего раза: {}", s.postponed)
            } else {
                String::new()
            }
        ),
        None => println!("Сейчас синхронизирует другой процесс ztaskmgr"),
    }
    if let Some(s) = &summary
        && !s.failed.is_empty()
    {
        bail!("не синхронизированы:\n  {}", s.failed.join("\n  "));
    }
    Ok(())
}

/// Через сколько после правки синхронизироваться: правки подряд уходят одной.
const SYNC_DELAY: Duration = Duration::from_secs(3);

fn run(terminal: &mut DefaultTerminal, app: &mut App, kitty: &mut bool, worker: &webdav::Worker) -> Result<()> {
    // Клавиши читаем сами, а не через crossterm: он теряет физическую клавишу (см. `input`).
    let mut reader = input::Reader::new();
    let mut size = terminal::size()?;
    app.on_resize(size.0);
    let mut time = clock::now();
    let mut dirty = true;
    // Мышь — это касания в Termux: тапы и свайпы. Пока открыто поле ввода, она выключена,
    // чтобы тап вызывал клавиатуру (см. `App::wants_text`).
    let mut mouse = false;
    let mut edited_at: Option<Instant> = None;
    while !app.quit {
        if mouse == app.wants_text() {
            mouse = !mouse;
            if mouse {
                input::mouse_on();
            } else {
                input::mouse_off();
            }
        }
        if dirty {
            app.prepare();
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
                Input::Mouse(mouse) => app.on_mouse(mouse),
            }
            dirty = true;
        }
        // Правки снаружи (агент через MCP, синхронизация, редактор) — до автосохранения,
        // чтобы оно их не затёрло; `tick` проверяет их и сам.
        if app.tick(Instant::now()) {
            dirty = true;
        }
        // WebDAV: правки — на сервер через пару секунд, новые настройки и «сейчас» — сразу.
        if std::mem::take(&mut app.edited) {
            edited_at = Some(Instant::now());
        }
        if edited_at.is_some_and(|t| t.elapsed() >= SYNC_DELAY) {
            edited_at = None;
            worker.now();
        }
        if std::mem::take(&mut app.webdav_changed) {
            // Команде пароля нужен терминал — на это время он ей отдаётся.
            let settings = match app.config.webdav() {
                Some(s) if s.needs_command() => released(terminal, *kitty, || s.resolve())?.map(Some),
                other => Ok(other),
            };
            dirty = true;
            match settings {
                Ok(settings) => worker.configure(settings),
                Err(e) => {
                    app.status = Some(Status::Error(format!("WebDAV: {e:#}")));
                    worker.configure(None);
                }
            }
        }
        if std::mem::take(&mut app.sync_requested) {
            worker.now();
        }
        for outcome in worker.outcomes() {
            app.on_sync(outcome);
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
            app.on_resize(size.0);
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

    let status = released(terminal, kitty, || Command::new(program).args(parts).arg(path).status())?;
    Ok(match status {
        Ok(s) if s.success() => None,
        Ok(s) => Some(format!("Редактор «{editor}» завершился с ошибкой: {s}")),
        Err(e) => Some(format!("Редактор «{editor}» не запустился: {e}")),
    })
}

/// Отдаёт терминал внешней программе на время `f` (редактор, команда пароля) и забирает
/// обратно.
fn released<T>(terminal: &mut DefaultTerminal, kitty: bool, f: impl FnOnce() -> T) -> Result<T> {
    if kitty {
        input::kitty_off();
    }
    input::mouse_off();
    execute!(std::io::stdout(), DisableBracketedPaste, LeaveAlternateScreen, cursor::Show)?;
    terminal::disable_raw_mode()?;
    let result = f();
    terminal::enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    input::mouse_on();
    if kitty {
        input::kitty_on();
    }
    terminal.clear()?;
    Ok(result)
}

/// `None` — запрошена справка.
/// Как запущена программа.
enum Run {
    Tui(PathBuf),
    /// MCP-сервер для ИИ-агентов.
    Mcp(PathBuf),
    /// Одна синхронизация с WebDAV.
    Sync(PathBuf),
}

fn parse_args() -> Result<Option<Run>> {
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(None);
    }
    let command = args.first().and_then(|a| a.to_str()).filter(|a| ["mcp", "sync"].contains(a)).map(str::to_string);
    if command.is_some() {
        args.remove(0);
    }
    let dir = match &args[..] {
        [] => default_path()?,
        [dir] => PathBuf::from(dir),
        _ => bail!("слишком много аргументов\n\n{USAGE}"),
    };
    Ok(Some(match command.as_deref() {
        Some("mcp") => Run::Mcp(dir),
        Some("sync") => Run::Sync(dir),
        _ => Run::Tui(dir),
    }))
}

/// Каталог книжки по умолчанию: `$ZTASKMGR_DIR`, иначе `~/.local/share/ztaskmgr`.
fn default_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("ZTASKMGR_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    ProjectDirs::from("", "", "ztaskmgr")
        .map(|dirs| dirs.data_dir().to_path_buf())
        .context("не удалось определить домашний каталог")
}
