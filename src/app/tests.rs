use super::*;
use ratatui::crossterm::event::KeyEvent;

fn d(day: u32) -> Date {
    Date::new(2026, 10, day).unwrap()
}

fn press(app: &mut App, code: KeyCode) {
    app.on_key(code);
}

fn press_with(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    app.on_key(KeyEvent::new(code, mods));
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c));
    }
}

/// Ctrl+S — сохранить многострочное поле.
fn save_note(app: &mut App) {
    press_with(app, KeyCode::Char('s'), KeyModifiers::CONTROL);
}

/// Программа на 26.10.2026; часы потока теста остановлены на этот день.
fn test_app(name: &str) -> App {
    clock::freeze_on(d(26));
    let dir = std::env::temp_dir().join(format!("ztaskmgr-app-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    App::new(Book::new(dir), Config::default(), d(26))
}

fn cleanup(app: App) {
    let _ = std::fs::remove_dir_all(app.book.dir());
}

fn texts(app: &App, category: ListId) -> Vec<String> {
    app.board.list(category).iter().map(|t| t.text.clone()).collect()
}

#[test]
fn add_move_toggle_delete() {
    let mut app = test_app("flow");

    for text in ["первая", "вторая"] {
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, text);
        press(&mut app, KeyCode::Enter);
    }
    assert_eq!(texts(&app, ListId::WORK), ["первая", "вторая"]);
    assert_eq!(app.selected(), Some(1));

    press_with(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    assert_eq!(texts(&app, ListId::WORK), ["вторая", "первая"]);
    assert_eq!(app.selected(), Some(0));
    // J/K больше не сдвигают.
    press(&mut app, KeyCode::Char('J'));
    assert_eq!(texts(&app, ListId::WORK), ["вторая", "первая"]);

    // Выполненная — вниз, выделение — на следующей задаче.
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(texts(&app, ListId::WORK), ["первая", "вторая"]);
    assert_eq!(app.board.list(ListId::WORK)[1].state, State::Done);
    assert_eq!(app.selected(), Some(0));
    // Снятая отметка — в конец открытых, выделение за ней.
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(app.board.list(ListId::WORK)[1].state, State::Open);
    assert_eq!(app.selected(), Some(1));
    press(&mut app, KeyCode::Char(' '));

    press(&mut app, KeyCode::Char('j'));
    // m — окно выбора без текущего списка: Дом, Личное, Прочее.
    press(&mut app, KeyCode::Char('m'));
    assert!(matches!(app.mode, Mode::MoveTo(0)));
    press(&mut app, KeyCode::Char('2'));
    assert_eq!(texts(&app, ListId::WORK), ["первая"]);
    assert_eq!(texts(&app, ListId::PERSONAL), ["вторая"]);
    assert_eq!(app.lists[&ListId::PERSONAL].selected(), Some(0));

    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(texts(&app, ListId::WORK), ["первая"]);
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('y'));
    assert!(app.board.list(ListId::WORK).is_empty());
    assert_eq!(app.selected(), None);

    assert_eq!(app.book.load(d(26)).unwrap(), app.board);
    assert_eq!(app.status, None);
    cleanup(app);
}

#[test]
fn cancel() {
    let mut app = test_app("cancel");
    for text in ["раз", "два"] {
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, text);
        press(&mut app, KeyCode::Enter);
    }
    press(&mut app, KeyCode::Char('g'));
    // Отменённая — вниз, выделение на следующей.
    press(&mut app, KeyCode::Char('c'));
    assert_eq!(texts(&app, ListId::WORK), ["два", "раз"]);
    assert_eq!(app.board.list(ListId::WORK)[1].state, State::Cancelled);
    assert_eq!(app.selected(), Some(0));
    assert_eq!(app.book.load(d(26)).unwrap(), app.board);
    // Повторно — снова открыта, выделение за ней; «с» в русской раскладке — та же клавиша.
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('с'));
    assert!(app.board.list(ListId::WORK)[1].is_open());
    assert_eq!(app.selected(), Some(1));
    // Отменённую всё-таки сделали.
    press(&mut app, KeyCode::Char('c'));
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(app.board.list(ListId::WORK)[1].state, State::Done);
    // Отменённая не переносится.
    press(&mut app, KeyCode::Char('c'));
    press(&mut app, KeyCode::Char('>'));
    assert!(matches!(app.mode, Mode::Normal));
    cleanup(app);
}

#[test]
fn russian_layout() {
    let mut app = test_app("ru");
    // «ф» — физическая a: новая задача; в поле ввода — сама «ф».
    press(&mut app, KeyCode::Char('ф'));
    type_text(&mut app, "фото");
    press(&mut app, KeyCode::Enter);
    assert_eq!(texts(&app, ListId::WORK), ["фото"]);
    // «ч» — x: выполнено; «в» + «н» — d + y: удалить.
    press(&mut app, KeyCode::Char('ч'));
    assert_eq!(app.board.list(ListId::WORK)[0].state, State::Done);
    press(&mut app, KeyCode::Char('в'));
    press(&mut app, KeyCode::Char('н'));
    assert!(app.board.list(ListId::WORK).is_empty());
    // Ctrl+W в поле ввода в русской раскладке.
    press(&mut app, KeyCode::Char('ф'));
    type_text(&mut app, "два слова");
    press_with(&mut app, KeyCode::Char('ц'), KeyModifiers::CONTROL);
    let Mode::Edit { input, .. } = &app.mode else { panic!() };
    assert_eq!(input.text(), "два ");
    // «х» / «ъ» — [ / ]: листание.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('ъ'));
    assert_eq!(app.date, d(27));
    cleanup(app);
}

#[test]
fn edit_and_cancel() {
    let mut app = test_app("edit");
    press(&mut app, KeyCode::Char('2'));
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "кран");
    press(&mut app, KeyCode::Enter);

    press(&mut app, KeyCode::Enter);
    type_text(&mut app, " течёт");
    press(&mut app, KeyCode::Esc);
    assert_eq!(texts(&app, ListId::HOME), ["кран"]);

    press(&mut app, KeyCode::Char('e'));
    press(&mut app, KeyCode::Home);
    app.on_paste("Починить\n ");
    press(&mut app, KeyCode::Enter);
    assert_eq!(texts(&app, ListId::HOME), ["Починить кран"]);

    // Пустой ввод не создаёт задачу.
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "   ");
    press(&mut app, KeyCode::Enter);
    assert_eq!(texts(&app, ListId::HOME).len(), 1);
    cleanup(app);
}

#[test]
fn pages_and_postpone() {
    let mut app = test_app("pages");
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "позвонить");
    press(&mut app, KeyCode::Enter);

    // Перенос: по умолчанию на завтра.
    press(&mut app, KeyCode::Char('>'));
    let Mode::Date { input, .. } = &app.mode else { panic!() };
    assert_eq!(input.text(), "2026-10-27");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.board.list(ListId::WORK)[0].state, State::Migrated(d(27)));
    assert!(matches!(app.status, Some(Status::Info(_))));
    // Перенесённую не отметить и не перенести второй раз.
    press(&mut app, KeyCode::Char(' '));
    assert_eq!(app.board.list(ListId::WORK)[0].state, State::Migrated(d(27)));
    press(&mut app, KeyCode::Char('>'));
    assert!(matches!(app.mode, Mode::Normal));

    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.date, d(27));
    assert_eq!(texts(&app, ListId::WORK), ["позвонить"]);
    assert!(app.board.list(ListId::WORK)[0].is_open());

    // Переход к дате: не дата — окно остаётся открытым.
    press(&mut app, KeyCode::Char('D'));
    type_text(&mut app, "31.02");
    press(&mut app, KeyCode::Enter);
    assert!(matches!(app.mode, Mode::Date { .. }));
    press_with(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    type_text(&mut app, "+3");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.date, d(30));
    assert!(app.board.is_empty());

    // Все задачи удалены — файла страницы нет.
    press(&mut app, KeyCode::Char('t'));
    assert_eq!(app.date, d(26));
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('y'));
    assert_eq!(app.book.dates().unwrap(), [d(27)]);
    cleanup(app);
}

#[test]
fn broken_page_is_read_only() {
    let app = test_app("broken");
    let path = app.book.dir().join("2026-10-25.md");
    std::fs::create_dir_all(app.book.dir()).unwrap();
    std::fs::write(&path, "## Работа\nпросто текст\n").unwrap();
    let mut app = app;
    press(&mut app, KeyCode::Char('['));
    assert!(app.page_error.is_some());
    press(&mut app, KeyCode::Char('a'));
    assert!(matches!(app.mode, Mode::Normal));
    assert!(matches!(app.status, Some(Status::Error(_))));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "## Работа\nпросто текст\n");
    press(&mut app, KeyCode::Char(']'));
    assert!(app.page_error.is_none() && app.status.is_none());
    cleanup(app);
}

#[test]
fn new_day_carries_over() {
    let mut app = test_app("midnight");
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "недоделано");
    press(&mut app, KeyCode::Enter);
    app.on_new_day(d(27));
    assert_eq!(app.date, d(27));
    assert_eq!(texts(&app, ListId::WORK), ["недоделано"]);
    assert!(matches!(app.status, Some(Status::Info(_))));
    press(&mut app, KeyCode::Char('['));
    assert_eq!(app.board.list(ListId::WORK)[0].state, State::Migrated(d(27)));
    cleanup(app);
}

#[test]
fn notes_window() {
    let mut app = test_app("notes");
    press(&mut app, KeyCode::Char('2'));
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "кран");
    press(&mut app, KeyCode::Enter);

    // id у задачи с самого начала; открыли и закрыли без записей — файла записей нет.
    let first_id = app.board.list(ListId::HOME)[0].id.clone().expect("id при создании");
    press(&mut app, KeyCode::Char('n'));
    assert!(app.notes.is_some());
    press(&mut app, KeyCode::Esc);
    assert!(app.notes.is_none());
    assert!(app.book.load_notes(&first_id).unwrap().is_empty());
    assert!(app.summaries.is_empty());

    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Char('s'));
    type_text(&mut app, "купить прокладку");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('a'));
    app.on_paste("мастер сказал");
    save_note(&mut app);
    // Горячие клавиши окна не трогают страницу: «x» — подзадача, а не задача.
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('x'));

    let id = app.board.list(ListId::HOME)[0].id.clone().unwrap();
    assert_eq!(id, first_id);
    assert_eq!(app.board.list(ListId::HOME)[0].state, State::Open);
    let notes = app.book.load_notes(&id).unwrap().serialize();
    let lines: Vec<&str> = notes.lines().collect();
    assert_eq!(lines[0], "- [x] купить прокладку");
    // Метка заметки — по часам (в тесте остановлены на 26.10.2026), а не по открытой странице.
    assert!(lines[1].starts_with("- 26.10.2026 ") && lines[1].ends_with(" мастер сказал"), "{notes}");
    assert_eq!(app.summaries[&id].subtasks_done, 1);
    // id сохранён на странице.
    assert_eq!(app.book.load(d(26)).unwrap(), app.board);

    // Сдвиг и правка строки.
    press_with(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Char('e'));
    press_with(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    type_text(&mut app, "смеситель");
    press(&mut app, KeyCode::Enter);
    let notes = app.book.load_notes(&id).unwrap().serialize();
    assert!(notes.ends_with("- [x] купить смеситель\n"), "{notes}");

    // Удаление строки с подтверждением.
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('y'));
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('y'));
    assert!(app.notes.as_ref().unwrap().notes.is_empty());
    assert!(!app.summaries.contains_key(&id));
    press(&mut app, KeyCode::Char('q'));
    assert!(!app.quit);
    cleanup(app);
}

#[test]
fn notes_follow_postpone_and_archive_on_delete() {
    let mut app = test_app("archive");
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "отчёт");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "начал");
    save_note(&mut app);
    press(&mut app, KeyCode::Esc);
    let id = app.board.list(ListId::WORK)[0].id.clone().unwrap();

    // Перенос — копия с теми же записями.
    press(&mut app, KeyCode::Char('>'));
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char(']'));
    assert_eq!(app.board.list(ListId::WORK)[0].id.as_deref(), Some(id.as_str()));
    assert!(app.summaries.contains_key(&id));

    // Удалили копию — запись [>] на 26-м осталась, записи на месте.
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('y'));
    assert!(app.book.load_notes(&id).unwrap().summary().any);
    // Удалили и её — записи ушли в DELETED_NOTES.md.
    press(&mut app, KeyCode::Char('['));
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('y'));
    assert!(app.book.load_notes(&id).unwrap().is_empty());
    let deleted = std::fs::read_to_string(app.book.dir().join(DELETED_NOTES)).unwrap();
    assert!(deleted.contains("— отчёт\n") && deleted.contains(" начал\n"), "{deleted}");
    assert!(matches!(app.status, Some(Status::Info(_))));
    cleanup(app);
}

#[test]
fn multiline_note() {
    let mut app = test_app("multiline");
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "смеситель");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Char('a'));
    // Enter — новая строка, пустые строки по краям отбрасываются.
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "мастер сказал:");
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "менять целиком");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Enter);
    app.on_paste("и ещё\r\nдве строки\n");
    press(&mut app, KeyCode::Enter);
    save_note(&mut app);

    let view = app.notes.as_ref().unwrap();
    assert!(matches!(view.mode, NotesMode::List));
    let item = view.notes.item(0);
    assert_eq!(item.head, "мастер сказал:");
    assert_eq!(item.body, ["менять целиком", "", "и ещё", "две строки"]);
    let id = app.board.list(ListId::WORK)[0].id.clone().unwrap();
    let file = app.book.load_notes(&id).unwrap().serialize();
    assert!(file.ends_with(" мастер сказал:\n  менять целиком\n\n  и ещё\n  две строки\n"), "{file}");

    // Правка — в многострочном поле с тем же текстом; Esc — отмена.
    press(&mut app, KeyCode::Char('e'));
    let Some(NotesView { mode: NotesMode::Text(Draft { area, .. }), .. }) = &app.notes else { panic!() };
    assert_eq!(area.text(), "мастер сказал:\nменять целиком\n\nи ещё\nдве строки");
    press(&mut app, KeyCode::Up);
    press(&mut app, KeyCode::End);
    type_text(&mut app, "!");
    save_note(&mut app);
    assert_eq!(app.notes.as_ref().unwrap().notes.item(0).body[2], "и ещё!");

    // Подзадача — однострочная: Enter сохраняет.
    press(&mut app, KeyCode::Char('s'));
    type_text(&mut app, "купить");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.notes.as_ref().unwrap().notes.visible().len(), 2);
    press(&mut app, KeyCode::Char('e'));
    assert!(matches!(app.notes.as_ref().unwrap().mode, NotesMode::Edit { .. }));
    press(&mut app, KeyCode::Esc);
    // Пустая заметка — не сохраняется.
    press(&mut app, KeyCode::Char('a'));
    press(&mut app, KeyCode::Enter);
    save_note(&mut app);
    assert_eq!(app.notes.as_ref().unwrap().notes.visible().len(), 2);
    cleanup(app);
}

/// Текст первой заметки в файле записей задачи из «Работы».
fn saved_note(app: &App) -> Option<String> {
    let id = app.board.list(ListId::WORK)[0].id.clone()?;
    let notes = app.book.load_notes(&id).unwrap();
    notes.visible().first().map(|&i| notes.editable_text(i))
}

fn draft(app: &App) -> &Draft {
    match &app.notes.as_ref().unwrap().mode {
        NotesMode::Text(draft) => draft,
        _ => panic!("ожидалось многострочное поле"),
    }
}

#[test]
fn note_autosave_and_close() {
    let mut app = test_app("autosave");
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "задача");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "черновик");

    // Раньше 10 секунд — не сохраняется, потом — сохраняется сама.
    assert!(!app.tick(Instant::now()));
    assert_eq!(saved_note(&app), None);
    assert!(app.tick(Instant::now() + AUTOSAVE));
    assert_eq!(saved_note(&app).as_deref(), Some("черновик"));
    assert!(draft(&app).saved_label.is_some() && !draft(&app).is_changed());

    // Дописали — Esc спрашивает; «другая клавиша» возвращает к правке.
    type_text(&mut app, " ещё");
    press(&mut app, KeyCode::Esc);
    assert!(draft(&app).confirm_close);
    press(&mut app, KeyCode::Char('x'));
    assert!(!draft(&app).confirm_close);
    assert_eq!(draft(&app).area.text(), "черновик ещё");

    // «y» — закрыть без последних изменений; сохранённое остаётся.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('y'));
    assert!(matches!(app.notes.as_ref().unwrap().mode, NotesMode::List));
    assert_eq!(saved_note(&app).as_deref(), Some("черновик"));
    // Заметка одна: автосохранение обновляет её, а не добавляет новую.
    assert_eq!(app.notes.as_ref().unwrap().notes.visible().len(), 1);

    // «s» в вопросе — сохранить и закрыть.
    press(&mut app, KeyCode::Char('e'));
    type_text(&mut app, "!");
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('s'));
    assert!(matches!(app.notes.as_ref().unwrap().mode, NotesMode::List));
    assert_eq!(saved_note(&app).as_deref(), Some("черновик!"));

    // Без изменений Esc закрывает сразу.
    press(&mut app, KeyCode::Char('e'));
    press(&mut app, KeyCode::Esc);
    assert!(matches!(app.notes.as_ref().unwrap().mode, NotesMode::List));

    // Стёрли всё: автосохранение и Esc + «y» заметку не трогают…
    press(&mut app, KeyCode::Char('e'));
    press_with(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert!(draft(&app).is_empty());
    assert!(!app.tick(Instant::now() + AUTOSAVE));
    press(&mut app, KeyCode::Esc);
    assert!(draft(&app).confirm_close);
    press(&mut app, KeyCode::Char('y'));
    assert_eq!(saved_note(&app).as_deref(), Some("черновик!"));
    // …а Ctrl+S — удаляет. Подзадача, стёртая и сохранённая Enter, — тоже.
    press(&mut app, KeyCode::Char('s'));
    type_text(&mut app, "пункт");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('e'));
    press_with(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    save_note(&mut app);
    assert!(matches!(app.notes.as_ref().unwrap().mode, NotesMode::List));
    let notes = &app.notes.as_ref().unwrap().notes;
    assert_eq!(notes.visible().len(), 1);
    assert_eq!(notes.editable_text(notes.visible()[0]), "пункт");
    press(&mut app, KeyCode::Char('e'));
    press_with(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    press(&mut app, KeyCode::Enter);
    assert!(app.notes.as_ref().unwrap().notes.is_empty());
    // Файла записей нет, значка у задачи нет.
    assert_eq!(saved_note(&app), None);
    assert!(app.summaries.is_empty());

    // Ctrl+C с начатой заметкой — сначала сохранить.
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "черновик!");
    save_note(&mut app);
    press(&mut app, KeyCode::Char('e'));
    type_text(&mut app, "?");
    press_with(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(app.quit);
    assert_eq!(saved_note(&app).as_deref(), Some("черновик!?"));
    cleanup(app);
}

#[test]
fn lists_window() {
    let mut app = test_app("lists");
    // L, a — новый список в конец; его клавиша — 5.
    press(&mut app, KeyCode::Char('L'));
    press(&mut app, KeyCode::Char('a'));
    assert!(app.wants_text());
    type_text(&mut app, "Ремонт");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.shown.len(), 5);
    let repair = app.shown[4];
    assert_eq!(app.list_title(repair), "Ремонт");
    assert!(matches!(app.mode, Mode::Lists { row: 4, sub: ListsMode::Browse }));
    // Занятое название — ошибка, поле остаётся открытым.
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "дом");
    press(&mut app, KeyCode::Enter);
    assert!(matches!(app.status, Some(Status::Error(_))));
    assert!(matches!(app.mode, Mode::Lists { sub: ListsMode::Add(_), .. }));
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Esc);
    assert!(matches!(app.mode, Mode::Normal));

    // Задачи в новом списке: сегодня и на прошлой странице.
    press(&mut app, KeyCode::Char('5'));
    assert_eq!(app.active, repair);
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "плитка");
    press(&mut app, KeyCode::Enter);
    let mut past = Board::default();
    past.push(repair, Task::new("замер"));
    app.book.save(d(25), &past).unwrap();

    // Переименовать, сдвинуть выше.
    press(&mut app, KeyCode::Char('L'));
    press(&mut app, KeyCode::Char('e'));
    press_with(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
    type_text(&mut app, "Ванная");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.list_title(repair), "Ванная");
    press_with(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    assert_eq!(app.registry.active()[3], repair);
    assert!(matches!(app.mode, Mode::Lists { row: 3, .. }));

    // Удалить: задачу с сегодняшней страницы — в выбранный список, сам список — в архив.
    press(&mut app, KeyCode::Char('d'));
    assert!(matches!(app.mode, Mode::Lists { sub: ListsMode::Delete { count: 1, .. }, .. }));
    press(&mut app, KeyCode::Char('1'));
    assert!(!app.registry.is_active(repair) && app.registry.get(repair).is_some());
    assert_eq!(texts(&app, ListId::WORK), ["плитка"]);
    assert!(!app.shown.contains(&repair));
    // На прошлой странице список виден как был.
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('['));
    assert!(app.shown.contains(&repair));
    assert_eq!(texts(&app, repair), ["замер"]);

    // Пустой список удаляется совсем, после подтверждения.
    press(&mut app, KeyCode::Char('L'));
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "Пустой");
    press(&mut app, KeyCode::Enter);
    let empty = *app.registry.active().last().unwrap();
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('y'));
    assert!(app.registry.get(empty).is_none());

    // Архивный — обратно на экран.
    let row = app.list_rows().iter().position(|&c| c == repair).unwrap();
    if let Mode::Lists { row: r, .. } = &mut app.mode {
        *r = row;
    }
    press(&mut app, KeyCode::Char('r'));
    assert!(app.registry.is_active(repair));
    // Всё сохранено в реестре.
    assert_eq!(app.book.lists().unwrap(), app.registry);
    cleanup(app);
}

#[test]
fn grid_navigation() {
    let mut app = test_app("grid");
    // Шесть списков — сетка 3×2: 1 2 3 / 4 5 6.
    for title in ["Пятый", "Шестой"] {
        press(&mut app, KeyCode::Char('L'));
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, title);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc);
    }
    app.width = 100;
    let at = |app: &App| app.shown.iter().position(|&c| c == app.active).unwrap();
    press(&mut app, KeyCode::Char('l'));
    press(&mut app, KeyCode::Right);
    assert_eq!(at(&app), 2);
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(at(&app), 2);
    press_with(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
    assert_eq!(at(&app), 5);
    press(&mut app, KeyCode::Char('h'));
    press_with(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
    assert_eq!(at(&app), 1);
    press(&mut app, KeyCode::Char('6'));
    assert_eq!(at(&app), 5);
    press(&mut app, KeyCode::Tab);
    assert_eq!(at(&app), 0);
    cleanup(app);
}

#[test]
fn save_does_not_overwrite_unseen_changes() {
    let mut app = test_app("unseen");
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "моя");
    press(&mut app, KeyCode::Enter);
    // Синхронизация в фоне записала страницу, программа ещё не перечитала её…
    let mut disk = app.book.load(d(26)).unwrap();
    disk.push(ListId::WORK, Task::new("с сервера"));
    std::thread::sleep(std::time::Duration::from_millis(5));
    app.book.save(d(26), &disk).unwrap();
    // …а пользователь уже правит: при сохранении чужая задача не пропадает.
    app.board.push(ListId::WORK, Task::new("ещё моя"));
    app.save();
    let texts: Vec<String> = app.book.load(d(26)).unwrap().list(ListId::WORK).iter().map(|t| t.text.clone()).collect();
    assert_eq!(texts, ["моя", "с сервера", "ещё моя"]);
    assert_eq!(app.book.load(d(26)).unwrap(), app.board);

    // Итог синхронизации с принесённым — автоперенос вчерашнего на сегодня.
    let mut yesterday = Board::default();
    yesterday.push(ListId::HOME, Task::new("со вчера"));
    app.book.save(d(25), &yesterday).unwrap();
    app.on_sync(Ok(crate::webdav::Summary { downloaded: 1, ..Default::default() }));
    assert!(app.board.list(ListId::HOME).iter().any(|t| t.text == "со вчера"), "{:?}", app.board);
    cleanup(app);
}

#[test]
fn webdav_settings() {
    let mut app = test_app("webdav");
    press(&mut app, KeyCode::Char('S'));
    // «Синхронизировать сейчас» без адреса — подсказка.
    for _ in 0..Setting::ALL.len() {
        press(&mut app, KeyCode::Char('j'));
    }
    press(&mut app, KeyCode::Enter);
    assert!(!app.sync_requested && app.status.is_some());
    // Адрес: Enter открывает поле, Enter сохраняет.
    for _ in 0..Setting::ALL.len() - 2 {
        press(&mut app, KeyCode::Char('k'));
    }
    press(&mut app, KeyCode::Enter);
    assert!(app.wants_text());
    type_text(&mut app, " https://dav.example/книжка ");
    press(&mut app, KeyCode::Enter);
    assert!(app.webdav_changed);
    assert_eq!(app.config.webdav().unwrap().url, "https://dav.example/книжка");
    // Пароль показывается точками; Esc в поле — без изменений.
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "секрет");
    press(&mut app, KeyCode::Enter);
    assert_eq!(Setting::WebdavPassword.value(&app.config).unwrap(), "••••••");
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "другой");
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.config.webdav().unwrap().password, "секрет");
    // Фраза шифрования — тоже точками.
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    type_text(&mut app, "фраза");
    press(&mut app, KeyCode::Enter);
    assert_eq!(Setting::WebdavPassphrase.value(&app.config).unwrap(), "•••••");
    assert_eq!(app.config.webdav().unwrap().passphrase, "фраза");
    // Смена фразы — просьба главному циклу; итог — в настройки.
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    assert!(app.wants_text());
    type_text(&mut app, "новая");
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.passphrase_change.take().as_deref(), Some("новая"));
    app.on_passphrase(Err("нет сети".into()));
    assert!(matches!(&app.status, Some(Status::Error(e)) if e.contains("не сменена")));
    assert_eq!(app.config.webdav().unwrap().passphrase, "фраза");
    app.on_passphrase(Ok("новая".into()));
    assert_eq!(app.config.webdav().unwrap().passphrase, "новая");
    // Теперь «сейчас» — просьба главному циклу.
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Enter);
    assert!(app.sync_requested);

    // Итог синхронизации: ошибка — один раз, успех после ошибки — сообщение.
    app.status = None;
    app.on_sync(Err("нет сети".into()));
    assert_eq!(app.status, Some(Status::Error("WebDAV: нет сети".into())));
    app.status = None;
    app.on_sync(Err("нет сети".into()));
    assert_eq!(app.status, None);
    app.on_sync(Ok(crate::webdav::Summary::default()));
    assert!(matches!(app.sync, SyncState::Ok(_)));
    assert_eq!(app.status, Some(Status::Info("WebDAV: снова на связи".into())));
    cleanup(app);
}

#[test]
fn privacy() {
    let mut app = test_app("privacy");
    press(&mut app, KeyCode::Char('3'));
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "личное дело");
    press(&mut app, KeyCode::Enter);
    // P — список скрыт, пометка в реестре списков.
    press(&mut app, KeyCode::Char('P'));
    assert!(app.is_hidden(ListId::PERSONAL));
    assert!(app.book.lists().unwrap().is_hidden(ListId::PERSONAL));

    // Из скрытого в открытый — задача уходит скрытой.
    press(&mut app, KeyCode::Char('m'));
    press(&mut app, KeyCode::Char('1'));
    let task = &app.board.list(ListId::WORK)[0];
    assert!(task.private);
    // p — открыть; из открытого в скрытый — пометка не нужна, скрыт сам список.
    press(&mut app, KeyCode::Char('1'));
    press(&mut app, KeyCode::Char('p'));
    assert!(!app.board.list(ListId::WORK)[0].private);
    press(&mut app, KeyCode::Char('m'));
    press(&mut app, KeyCode::Char('2'));
    assert!(!app.board.list(ListId::PERSONAL)[0].private);
    // Пометка сохраняется в файле и едет при переносе на дату.
    press(&mut app, KeyCode::Char('3'));
    press(&mut app, KeyCode::Char('m'));
    press(&mut app, KeyCode::Char('2'));
    press(&mut app, KeyCode::Char('2'));
    assert!(app.board.list(ListId::HOME)[0].private);
    press(&mut app, KeyCode::Char('>'));
    press(&mut app, KeyCode::Enter);
    assert!(app.book.load(d(27)).unwrap().list(ListId::HOME)[0].private);
    cleanup(app);
}

#[test]
fn deleted_entries_are_archived() {
    let mut app = test_app("entries");
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "кран");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Char('s'));
    type_text(&mut app, "прокладка");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('d'));
    press(&mut app, KeyCode::Char('y'));
    let deleted = std::fs::read_to_string(app.book.dir().join(DELETED_NOTES)).unwrap();
    assert!(deleted.contains("— кран · удалённый пункт записей\n"), "{deleted}");
    assert!(deleted.contains("- [ ] прокладка\n"), "{deleted}");
    cleanup(app);
}

#[test]
fn external_changes_are_picked_up() {
    let mut app = test_app("external");
    for text in ["первая", "вторая"] {
        press(&mut app, KeyCode::Char('a'));
        type_text(&mut app, text);
        press(&mut app, KeyCode::Enter);
    }
    assert_eq!(app.selected(), Some(1));
    assert!(!app.check_external());

    // Агент дописал задачу в начало — выделение остаётся на «вторая».
    let mut board = app.book.load(d(26)).unwrap();
    board.push(ListId::WORK, Task::new("от агента"));
    board.move_up(ListId::WORK, 2);
    board.move_up(ListId::WORK, 1);
    app.book.save(d(26), &board).unwrap();
    assert!(app.check_external());
    assert_eq!(texts(&app, ListId::WORK), ["от агента", "первая", "вторая"]);
    assert_eq!(app.selected(), Some(2));
    assert!(matches!(app.status, Some(Status::Info(_))));

    // Правка задачи, пока агент переставил задачи: правится та же задача.
    press(&mut app, KeyCode::Char('e'));
    let mut board = app.book.load(d(26)).unwrap();
    board.move_up(ListId::WORK, 2);
    app.book.save(d(26), &board).unwrap();
    type_text(&mut app, "!");
    press(&mut app, KeyCode::Enter);
    assert_eq!(texts(&app, ListId::WORK), ["от агента", "вторая!", "первая"]);

    // Записи: агент добавил заметку, пока пишем свою, — обе на месте.
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "моя");
    assert!(app.tick(Instant::now() + AUTOSAVE));
    let id = app.board.list(ListId::WORK)[1].id.clone().unwrap();
    let mut notes = app.book.load_notes(&id).unwrap();
    notes.add_note("26.10.2026 09:00", "заметка агента");
    app.book.save_notes(&id, &notes).unwrap();
    type_text(&mut app, " длиннее");
    save_note(&mut app);
    let file = app.book.load_notes(&id).unwrap().serialize();
    assert!(file.contains(" моя длиннее\n") && file.contains(" заметка агента\n"), "{file}");
    assert!(!file.contains(" моя\n"), "{file}");

    // Задачу дописали снаружи без id (руками на телефоне); у неё открыто окно записей,
    // а агент дал ей id и запись — окно на месте, запись видна.
    press(&mut app, KeyCode::Esc);
    let mut board = app.book.load(d(26)).unwrap();
    board.push(ListId::HOME, Task::new("снаружи"));
    app.book.save(d(26), &board).unwrap();
    press(&mut app, KeyCode::Char('2'));
    press(&mut app, KeyCode::Char('n'));
    assert!(app.board.list(ListId::HOME)[0].id.is_none());
    let mut board = app.book.load(d(26)).unwrap();
    board.get_mut(ListId::HOME, 0).unwrap().id = Some("agent1".into());
    app.book.save(d(26), &board).unwrap();
    let mut notes = crate::notes::Notes::default();
    notes.add_subtask("от агента");
    app.book.save_notes("agent1", &notes).unwrap();
    assert!(app.check_external());
    let view = app.notes.as_ref().expect("окно записей осталось");
    assert_eq!(view.id, "agent1");
    assert_eq!(view.notes.visible().len(), 1);
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('1'));
    press(&mut app, KeyCode::Char('g'));
    press(&mut app, KeyCode::Char('j'));
    press(&mut app, KeyCode::Char('n'));

    // Задачу удалили снаружи — окно записей закрылось.
    let mut board = app.book.load(d(26)).unwrap();
    board.remove(ListId::WORK, 1);
    app.book.save(d(26), &board).unwrap();
    press(&mut app, KeyCode::Char('j'));
    assert!(app.notes.is_none());
    assert_eq!(texts(&app, ListId::WORK), ["от агента", "первая"]);
    cleanup(app);
}

#[test]
fn editor_round_trip() {
    let mut app = test_app("editor");
    press(&mut app, KeyCode::Char('a'));
    type_text(&mut app, "длинное");
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Char('E'));
    let path = app.editor_request.take().expect("запрос редактора");
    // «Редактор» записал файл.
    std::fs::write(&path, "Абзац из редактора\n- [ ] пункт\n").unwrap();
    app.after_editor(None);
    let task = &app.board.list(ListId::WORK)[0];
    let id = task.id.clone().expect("id после редактора");
    assert!(path.ends_with(format!("{id}.md")));
    assert_eq!(app.summaries[&id].subtasks, 1);
    assert_eq!(app.notes.as_ref().unwrap().notes.visible().len(), 2);
    // Редактор не запустился — ошибка в строке состояния, окно на месте.
    app.after_editor(Some("Редактор «nope» не запустился".into()));
    assert!(matches!(app.status, Some(Status::Error(_))));
    assert!(app.notes.is_some());
    cleanup(app);
}

#[test]
fn text_fields_release_the_mouse() {
    let mut app = test_app("keyboard");
    assert!(!app.wants_text());
    press(&mut app, KeyCode::Char('a'));
    assert!(app.wants_text());
    type_text(&mut app, "x");
    press(&mut app, KeyCode::Enter);
    assert!(!app.wants_text());
    press(&mut app, KeyCode::Char('n'));
    assert!(!app.wants_text());
    press(&mut app, KeyCode::Char('a'));
    assert!(app.wants_text());
    type_text(&mut app, "заметка");
    press(&mut app, KeyCode::Esc);
    // Вопрос «закрыть?» — кнопки-подсказки снова нужны.
    assert!(!app.wants_text());
    cleanup(app);
}

#[test]
fn help_and_about_close_on_any_key() {
    let mut app = test_app("help");
    press(&mut app, KeyCode::Char('?'));
    assert!(app.show_help);
    // Клавиша, закрывшая окно, ничего больше не делает.
    press(&mut app, KeyCode::Char('q'));
    assert!(!app.show_help && !app.quit);
    press(&mut app, KeyCode::F(1));
    assert!(app.show_help);
    press(&mut app, KeyCode::Esc);
    press(&mut app, KeyCode::Char('o'));
    assert!(app.show_about);
    press(&mut app, KeyCode::Char('a'));
    assert!(!app.show_about && matches!(app.mode, Mode::Normal));
}

#[test]
fn navigation_stays_in_bounds() {
    let mut app = test_app("nav");
    for text in ["a", "b"] {
        app.board.push(ListId::WORK, Task::new(text));
    }
    app.select(ListId::WORK, Some(0));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.selected(), Some(0));
    for _ in 0..5 {
        press(&mut app, KeyCode::Char('j'));
    }
    assert_eq!(app.selected(), Some(1));
    press(&mut app, KeyCode::Char('g'));
    assert_eq!(app.selected(), Some(0));
    press(&mut app, KeyCode::Char('G'));
    assert_eq!(app.selected(), Some(1));
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.active, ListId::HOME);
    assert_eq!(app.selected(), None);
    press(&mut app, KeyCode::Up);
    assert_eq!(app.selected(), None);
}
