use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::clock::Date;
use crate::model::{Board, Category, State, Task};

/// Разбирает Markdown-файл задач.
///
/// Файл перезаписывается целиком, поэтому неизвестные строки — ошибка, а не пропуск:
/// иначе правки, сделанные руками, молча потеряются при первом сохранении.
pub fn parse(text: &str) -> Result<Board> {
    let mut board = Board::default();
    let mut current = None;

    for (n, line) in text.lines().enumerate() {
        let n = n + 1;
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }

        if let Some(title) = line.strip_prefix("## ") {
            let category = Category::from_title(title.trim())
                .with_context(|| format!("строка {n}: неизвестная категория «{}»", title.trim()))?;
            current = Some(category);
            continue;
        }

        let Some(task) = parse_task(line) else {
            bail!("строка {n}: ожидалась задача «- [ ] …», «- [x] …», «- [>] … → ГГГГ-ММ-ДД» или заголовок «## …»: {line}");
        };
        let Some(category) = current else {
            bail!("строка {n}: задача до первого заголовка категории");
        };
        board.push(category, task);
    }

    Ok(board)
}

/// `<!-- id:k3x9ab -->` в конце строки задачи — ключ её доп. записей.
fn split_id(line: &str) -> Option<(&str, Option<String>)> {
    let Some(rest) = line.strip_suffix(" -->") else {
        return Some((line, None));
    };
    let Some((line, id)) = rest.rsplit_once(" <!-- id:") else {
        return Some((line, None));
    };
    is_valid_id(id).then(|| (line, Some(id.to_string())))
}

/// id записей: латинские строчные буквы и цифры (он же имя файла).
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 16 && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn parse_task(line: &str) -> Option<Task> {
    let (line, id) = split_id(line)?;
    let rest = line.strip_prefix("- [")?;
    let (mark, text) = rest.split_once("] ").or_else(|| {
        // Пустой текст: «- [ ]» без пробела в конце после trim_end.
        rest.strip_suffix(']').map(|mark| (mark, ""))
    })?;
    let (state, text) = match mark {
        " " => (State::Open, text),
        "x" | "X" => (State::Done, text),
        ">" => {
            let (text, date) = text.rsplit_once(MIGRATED_SEP)?;
            (State::Migrated(Date::parse_iso(date)?), text)
        }
        _ => return None,
    };
    Some(Task {
        text: text.to_string(),
        state,
        id,
    })
}

/// Между текстом перенесённой задачи и датой, на которую её перенесли.
const MIGRATED_SEP: &str = " → ";

pub fn serialize(board: &Board) -> String {
    let mut out = String::new();
    for (i, category) in Category::ALL.into_iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str("## ");
        out.push_str(category.title());
        out.push('\n');
        for task in board.list(category) {
            out.push_str(match task.state {
                State::Open => "- [ ] ",
                State::Done => "- [x] ",
                State::Migrated(_) => "- [>] ",
            });
            out.push_str(&task.text);
            if let State::Migrated(date) = task.state {
                out.push_str(MIGRATED_SEP);
                out.push_str(&date.to_string());
            }
            if let Some(id) = &task.id {
                out.push_str(" <!-- id:");
                out.push_str(id);
                out.push_str(" -->");
            }
            out.push('\n');
        }
    }
    out
}

/// Загружает доску; отсутствующий файл — пустая доска.
pub fn load(path: &Path) -> Result<Board> {
    match fs::read_to_string(path) {
        Ok(text) => parse(&text).with_context(|| format!("разбор {}", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Board::default()),
        Err(e) => Err(e).with_context(|| format!("чтение {}", path.display())),
    }
}

pub fn save(path: &Path, board: &Board) -> Result<()> {
    write_atomic(path, &serialize(board))
}

/// Пишет через временный файл и rename, чтобы сбой посреди записи не испортил данные.
pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).with_context(|| format!("создание {}", dir.display()))?;
    }

    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = Path::new(&tmp_name);

    let mut file = fs::File::create(tmp).with_context(|| format!("запись {}", tmp.display()))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);

    fs::rename(tmp, path).with_context(|| format!("переименование в {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
## Работа
- [ ] Написать отчёт
- [x] Созвон с командой

## Дом
- [ ] Починить кран
- [>] Позвонить мастеру → 2026-10-27 <!-- id:k3x9ab -->
- [ ] С записями <!-- id:q1 -->

## Личное

## Прочее
- [X] Что-то ещё
";

    #[test]
    fn parse_sample() {
        let board = parse(SAMPLE).unwrap();
        let work = board.list(Category::Work);
        assert_eq!(work.len(), 2);
        assert_eq!(work[0], Task::new("Написать отчёт"));
        assert_eq!(work[1].state, State::Done);
        let home = board.list(Category::Home);
        assert_eq!(home.len(), 3);
        assert_eq!(home[1].text, "Позвонить мастеру");
        assert_eq!(home[1].id.as_deref(), Some("k3x9ab"));
        assert_eq!(home[2].text, "С записями");
        assert_eq!(home[2].id.as_deref(), Some("q1"));
        assert_eq!(home[0].id, None);
        assert_eq!(home[1].state, State::Migrated(Date::new(2026, 10, 27).unwrap()));
        assert!(board.list(Category::Personal).is_empty());
        assert_eq!(board.list(Category::Other)[0].state, State::Done);
    }

    #[test]
    fn roundtrip() {
        let board = parse(SAMPLE).unwrap();
        let text = serialize(&board);
        // [X] нормализуется в [x], остальное совпадает байт в байт.
        assert_eq!(text, SAMPLE.replace("[X]", "[x]"));
        assert_eq!(parse(&text).unwrap(), board);
    }

    #[test]
    fn empty_board_has_all_headings() {
        let text = serialize(&Board::default());
        assert_eq!(text, "## Работа\n\n## Дом\n\n## Личное\n\n## Прочее\n");
    }

    #[test]
    fn text_keeps_brackets_and_spaces() {
        let board = parse("## Дом\n- [ ] купить [молоко]  и хлеб\n- [ ]\n").unwrap();
        let home = board.list(Category::Home);
        assert_eq!(home[0].text, "купить [молоко]  и хлеб");
        assert_eq!(home[1].text, "");
    }

    #[test]
    fn errors_have_line_numbers() {
        let err = parse("## Работа\nпросто текст\n").unwrap_err();
        assert!(err.to_string().contains("строка 2"), "{err}");

        let err = parse("## Отпуск\n").unwrap_err();
        assert!(err.to_string().contains("Отпуск"), "{err}");

        let err = parse("- [ ] сирота\n").unwrap_err();
        assert!(err.to_string().contains("строка 1"), "{err}");

        assert!(parse("## Работа\n- [?] странная\n").is_err());
        // Перенесённая — только с датой.
        assert!(parse("## Работа\n- [>] без даты\n").is_err());
        assert!(parse("## Работа\n- [>] плохая → 2026-13-01\n").is_err());
        let board = parse("## Работа\n- [>] a → b → 2026-10-27\n").unwrap();
        assert_eq!(board.list(Category::Work)[0].text, "a → b");
        // Испорченный id — ошибка, а не часть текста.
        assert!(parse("## Работа\n- [ ] x <!-- id:Bad! -->\n").is_err());
        // Обычный комментарий в тексте — текст.
        let board = parse("## Работа\n- [ ] x <!-- не id -->\n").unwrap();
        assert_eq!(board.list(Category::Work)[0].text, "x <!-- не id -->");
    }

    #[test]
    fn save_and_load() {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-test-{}", std::process::id()));
        let path = dir.join("nested/tasks.md");
        assert_eq!(load(&path).unwrap(), Board::default());

        let board = parse(SAMPLE).unwrap();
        save(&path, &board).unwrap();
        assert_eq!(load(&path).unwrap(), board);
        assert!(!dir.join("nested/tasks.md.tmp").exists());

        fs::remove_dir_all(&dir).unwrap();
    }
}
