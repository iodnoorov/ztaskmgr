use anyhow::{Context, Result, bail};

use crate::clock::Date;
use crate::lists::Lists;
use crate::model::{Board, ListId, State, Task};

/// Заголовок списка: `## Работа <!-- list:work -->`. Без id (страницы старых версий, правка
/// руками) — список по названию из реестра. Неизвестное название — ошибка.
fn parse_heading(title: &str, lists: &Lists) -> Option<(ListId, String)> {
    let title = title.trim();
    if let Some(rest) = title.strip_suffix(" -->")
        && let Some((title, comment)) = rest.rsplit_once(" <!-- ")
        && let Some(id) = comment.trim().strip_prefix("list:")
    {
        return is_valid_id(id).then(|| (ListId::new(id), title.trim().to_string()));
    }
    Some((lists.resolve_heading(title)?, title.to_string()))
}

/// Разбирает Markdown-файл задач; названия списков без id ищутся в реестре `lists`.
///
/// Файл перезаписывается целиком, поэтому неизвестные строки — ошибка, а не пропуск:
/// иначе правки, сделанные руками, молча потеряются при первом сохранении.
pub fn parse(text: &str, lists: &Lists) -> Result<Board> {
    let mut board = Board::default();
    let mut current = None;

    for (n, line) in text.lines().enumerate() {
        let n = n + 1;
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }

        if let Some(graves) = line.strip_prefix(GRAVES).and_then(|l| l.strip_suffix(" -->")) {
            for grave in graves.split_whitespace() {
                let parsed = grave.split_once('@').and_then(|(id, t)| Some((id, t.parse::<u64>().ok()?)));
                match parsed {
                    Some((id, t)) if is_valid_id(id) => board.bury(id, t),
                    _ => bail!("строка {n}: испорченное надгробие «{grave}»"),
                }
            }
            continue;
        }

        if let Some(title) = line.strip_prefix("## ") {
            let (category, title) = parse_heading(title, lists)
                .with_context(|| format!("строка {n}: неизвестный список «{}»", title.trim()))?;
            board.set_title(category, &title);
            current = Some(category);
            continue;
        }

        let Some(task) = parse_task(line) else {
            bail!(
                "строка {n}: ожидалась задача «- [ ] …», «- [x] …», «- [>] … → ГГГГ-ММ-ДД» или заголовок «## …»: {line}"
            );
        };
        let Some(category) = current else {
            bail!("строка {n}: задача до первого заголовка списка");
        };
        board.push(category, task);
    }

    Ok(board)
}

/// Строка надгробий удалённых задач в конце страницы: `<!-- deleted: abc@1730112345678 -->`.
const GRAVES: &str = "<!-- deleted: ";

/// Служебное в конце строки задачи: `<!-- id:k3x9ab t:1730112345678 private -->` — ключ
/// доп. записей, время правки и пометка «скрыта от ИИ», в любом наборе. Незнакомые пометки
/// `ключ:значение` (от более новой версии) сохраняются; комментарий из других слов — часть
/// текста.
#[derive(Default)]
struct Meta {
    id: Option<String>,
    private: bool,
    modified: u64,
    extra: Vec<String>,
}

fn split_meta(line: &str) -> Option<(&str, Meta)> {
    let plain = Some((line, Meta::default()));
    let Some(rest) = line.strip_suffix(" -->") else {
        return plain;
    };
    let Some((text, comment)) = rest.rsplit_once(" <!-- ") else {
        return plain;
    };
    let mut meta = Meta::default();
    for word in comment.split_whitespace() {
        let (key, value) = word.split_once(':').unwrap_or((word, ""));
        match (key, value) {
            // Испорченный id — ошибка, а не текст: иначе записи потеряют задачу.
            ("id", id) if !is_valid_id(id) => return None,
            ("id", id) => meta.id = Some(id.to_string()),
            ("t", t) => match t.parse() {
                Ok(t) => meta.modified = t,
                Err(_) => return plain,
            },
            ("private", "") => meta.private = true,
            (key, value) if !value.is_empty() && !key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase()) => {
                meta.extra.push(word.to_string());
            }
            _ => return plain,
        }
    }
    Some((text, meta))
}

/// id записей: латинские строчные буквы и цифры (он же имя файла).
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 16 && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn parse_task(line: &str) -> Option<Task> {
    let (line, Meta { id, private, modified, extra }) = split_meta(line)?;
    let rest = line.strip_prefix("- [")?;
    let (mark, text) = rest.split_once("] ").or_else(|| {
        // Пустой текст: «- [ ]» без пробела в конце после trim_end.
        rest.strip_suffix(']').map(|mark| (mark, ""))
    })?;
    let (state, text) = match mark {
        " " => (State::Open, text),
        "x" | "X" => (State::Done, text),
        "-" => (State::Cancelled, text),
        ">" => {
            let (text, date) = text.rsplit_once(MIGRATED_SEP)?;
            (State::Migrated(Date::parse_iso(date)?), text)
        }
        _ => return None,
    };
    Some(Task { text: text.to_string(), state, id, private, modified, extra })
}

/// Между текстом перенесённой задачи и датой, на которую её перенесли.
const MIGRATED_SEP: &str = " → ";

/// Списки с задачами: сначала те, что на экране, в их порядке, затем остальные (архивные,
/// незнакомые) — как на странице. Название — из реестра, иначе — из заголовка файла.
pub fn serialize(board: &Board, lists: &Lists) -> String {
    let order: Vec<ListId> = lists.order(board.ids()).into_iter().filter(|&id| !board.list(id).is_empty()).collect();
    let mut out = String::new();
    for (i, category) in order.into_iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let title = lists.title(category).or(board.title(category)).unwrap_or(category.as_str());
        out.push_str(&format!("## {title} <!-- list:{} -->\n", category.as_str()));
        for task in board.list(category) {
            out.push_str(match task.state {
                State::Open => "- [ ] ",
                State::Done => "- [x] ",
                State::Cancelled => "- [-] ",
                State::Migrated(_) => "- [>] ",
            });
            out.push_str(&task.text);
            if let State::Migrated(date) = task.state {
                out.push_str(MIGRATED_SEP);
                out.push_str(&date.to_string());
            }
            let mut meta: Vec<String> = task.id.iter().map(|id| format!("id:{id}")).collect();
            if task.modified > 0 {
                meta.push(format!("t:{}", task.modified));
            }
            if task.private {
                meta.push("private".into());
            }
            meta.extend(task.extra.iter().cloned());
            if !meta.is_empty() {
                out.push_str(&format!(" <!-- {} -->", meta.join(" ")));
            }
            out.push('\n');
        }
    }
    if !board.graves().is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        let graves: Vec<String> = board.graves().iter().map(|(id, t)| format!("{id}@{t}")).collect();
        out.push_str(&format!("{GRAVES}{} -->\n", graves.join(" ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Страницы тестов — со списками новой книжки.
    fn parse(text: &str) -> Result<Board> {
        super::parse(text, &Lists::default())
    }

    fn serialize(board: &Board) -> String {
        super::serialize(board, &Lists::default())
    }

    const SAMPLE: &str = "\
## Работа
- [ ] Написать отчёт
- [x] Созвон с командой

## Дом
- [ ] Починить кран
- [>] Позвонить мастеру → 2026-10-27 <!-- id:k3x9ab -->
- [ ] С записями <!-- id:q1 -->
- [ ] Секрет <!-- id:s2 private -->
- [ ] Просто скрытая <!-- private -->

## Личное
- [-] Передумал <!-- id:c3 -->

## Прочее
- [X] Что-то ещё
";

    #[test]
    fn parse_sample() {
        let board = parse(SAMPLE).unwrap();
        let work = board.list(ListId::WORK);
        assert_eq!(work.len(), 2);
        assert_eq!(work[0], Task::new("Написать отчёт"));
        assert_eq!(work[1].state, State::Done);
        let home = board.list(ListId::HOME);
        assert_eq!(home.len(), 5);
        assert!(home[3].private && home[3].id.as_deref() == Some("s2"));
        assert!(home[4].private && home[4].id.is_none() && home[4].text == "Просто скрытая");
        assert!(!home[2].private);
        assert_eq!(home[1].text, "Позвонить мастеру");
        assert_eq!(home[1].id.as_deref(), Some("k3x9ab"));
        assert_eq!(home[2].text, "С записями");
        assert_eq!(home[2].id.as_deref(), Some("q1"));
        assert_eq!(home[0].id, None);
        assert_eq!(home[1].state, State::Migrated(Date::new(2026, 10, 27).unwrap()));
        assert_eq!(board.list(ListId::PERSONAL)[0].state, State::Cancelled);
        assert_eq!(board.list(ListId::OTHER)[0].state, State::Done);
    }

    #[test]
    fn roundtrip() {
        let board = parse(SAMPLE).unwrap();
        let text = serialize(&board);
        // Заголовки получают id списков, [X] нормализуется в [x], остальное — как было.
        let expected = SAMPLE
            .replace("[X]", "[x]")
            .replace("## Работа\n", "## Работа <!-- list:work -->\n")
            .replace("## Дом\n", "## Дом <!-- list:home -->\n")
            .replace("## Личное\n", "## Личное <!-- list:personal -->\n")
            .replace("## Прочее\n", "## Прочее <!-- list:other -->\n");
        assert_eq!(text, expected);
        assert_eq!(parse(&text).unwrap(), board);
    }

    #[test]
    fn times_extras_and_graves() {
        let text = "## Работа <!-- list:work -->\n- [ ] a <!-- id:aa t:1730112345678 future:yes -->\n- [ ] b <!-- t:вчера -->\n\n<!-- deleted: bb@1730000000000 cc@5 -->\n";
        let board = parse(text).unwrap();
        let a = &board.list(ListId::WORK)[0];
        assert_eq!((a.modified, a.extra.as_slice()), (1730112345678, ["future:yes".to_string()].as_slice()));
        // Не число после t: — обычный комментарий, часть текста.
        assert_eq!(board.list(ListId::WORK)[1].text, "b <!-- t:вчера -->");
        assert_eq!(board.buried("bb"), Some(1730000000000));
        assert_eq!(serialize(&board), text);
        assert!(parse("## Работа\n<!-- deleted: Bad!@1 -->\n").is_err());
    }

    #[test]
    fn lists_by_id_and_title() {
        // Пустых списков в файле нет; пустая страница — пустой текст.
        assert_eq!(serialize(&Board::default()), "");
        let mut lists = Lists::default();
        lists.rename(ListId::WORK, "Офис").unwrap();
        let repair = ListId::new("k3x9ab");
        lists.add(repair, "Ремонт").unwrap();
        // Старый заголовок «Работа» — исходный список, хоть его и переименовали; новый —
        // по названию; незнакомый id — с названием из файла.
        let text = "## Работа\n- [ ] a\n\n## ремонт\n- [ ] b\n\n## Отпуск <!-- list:zz9 -->\n- [ ] c\n";
        let board = super::parse(text, &lists).unwrap();
        assert_eq!(board.list(ListId::WORK)[0].text, "a");
        assert_eq!(board.list(repair)[0].text, "b");
        let trip = ListId::new("zz9");
        assert_eq!((board.list(trip)[0].text.as_str(), board.title(trip)), ("c", Some("Отпуск")));
        // Порядок — как в реестре, незнакомые — после; названия — из реестра.
        assert_eq!(
            super::serialize(&board, &lists),
            "## Офис <!-- list:work -->\n- [ ] a\n\n## Ремонт <!-- list:k3x9ab -->\n- [ ] b\n\n## Отпуск <!-- list:zz9 -->\n- [ ] c\n"
        );
        assert!(super::parse("## Ремонт <!-- list:Bad! -->\n", &lists).is_err());
    }

    #[test]
    fn text_keeps_brackets_and_spaces() {
        let board = parse("## Дом\n- [ ] купить [молоко]  и хлеб\n- [ ]\n").unwrap();
        let home = board.list(ListId::HOME);
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
        assert_eq!(board.list(ListId::WORK)[0].text, "a → b");
        // Испорченный id — ошибка, а не часть текста.
        assert!(parse("## Работа\n- [ ] x <!-- id:Bad! -->\n").is_err());
        // Обычный комментарий в тексте — текст.
        let board = parse("## Работа\n- [ ] x <!-- не id -->\n").unwrap();
        assert_eq!(board.list(ListId::WORK)[0].text, "x <!-- не id -->");
    }
}
