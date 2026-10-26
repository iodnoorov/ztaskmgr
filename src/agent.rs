//! Книжка глазами ИИ-агента: чтение и правка с учётом скрытого. Скрытые списки
//! (`AI_HIDDEN.md`) и скрытые задачи агент не видит вовсе — ни в выдаче, ни в поиске,
//! ни по ссылке, а писать в скрытые списки не может. Нумерация задач и пунктов записей —
//! только среди видимого, чтобы по пропускам нельзя было догадаться о скрытом.
//! Каждая правка — прочитать с диска, изменить, записать: открытая программа подхватит её сама.

use anyhow::{Context, Result, bail};

use crate::book::Book;
use crate::clock::{self, Date};
use crate::model::{Board, Category, State, Task};
use crate::notes::{Kind, Notes};

pub struct Agent {
    book: Book,
}

/// Найденная по ссылке задача: страница (уже прочитанная) и место на ней.
struct Found {
    date: Date,
    category: Category,
    index: usize,
    board: Board,
}

impl Found {
    fn task(&self) -> &Task {
        &self.board.list(self.category)[self.index]
    }

    fn task_mut(&mut self) -> &mut Task {
        self.board.get_mut(self.category, self.index).unwrap()
    }
}

/// Список по названию: русскому или английскому, без учёта регистра.
pub fn parse_list(name: &str) -> Result<Category> {
    let name = name.trim().to_lowercase();
    let english = ["work", "home", "personal", "other"];
    Category::ALL
        .into_iter()
        .find(|c| c.title().to_lowercase() == name || english[c.index()] == name)
        .with_context(|| format!("unknown list «{name}»: use Работа/work, Дом/home, Личное/personal, Прочее/other"))
}

/// Дата: `today` / `tomorrow` / `yesterday` (и по-русски), `2026-10-27`, `27.10.2026`, `27.10`,
/// `+1` / `-3` — от сегодня.
pub fn parse_date(text: &str, today: Date) -> Result<Date> {
    let text = text.trim();
    match text.to_lowercase().as_str() {
        "today" | "сегодня" => return Ok(today),
        "tomorrow" | "завтра" => return Ok(today.add_days(1)),
        "yesterday" | "вчера" => return Ok(today.add_days(-1)),
        _ => {}
    }
    Date::parse_input(text, today)
        .with_context(|| format!("not a date «{text}»: use YYYY-MM-DD, DD.MM, +N, today"))
}

fn state_mark(state: State) -> String {
    match state {
        State::Open => "[ ]".into(),
        State::Done => "[x]".into(),
        State::Migrated(to) => format!("[>] (moved to {to})"),
    }
}

impl Agent {
    pub fn new(book: Book) -> Self {
        Self { book }
    }

    #[cfg(test)]
    pub fn book(&self) -> &Book {
        &self.book
    }

    fn hidden(&self) -> Result<[bool; 4]> {
        self.book.hidden()
    }

    fn visible(task: &Task, category: Category, hidden: &[bool; 4]) -> bool {
        !hidden[category.index()] && !task.private
    }

    /// Видимые задачи списка: (номер среди видимых с 1, индекс в списке).
    fn visible_indices(board: &Board, category: Category, hidden: &[bool; 4]) -> Vec<usize> {
        let list = board.list(category);
        (0..list.len())
            .filter(|&i| Self::visible(&list[i], category, hidden))
            .collect()
    }

    /// Ссылка на задачу в выдаче: id, если есть, иначе `дата/список/номер`.
    fn task_ref(date: Date, category: Category, number: usize, task: &Task) -> String {
        match &task.id {
            Some(id) => id.clone(),
            None => format!("{date}/{}/{number}", category.title()),
        }
    }

    /// Задача по ссылке: id (берётся живая копия — не `[>]`, с самой поздней страницы) или
    /// `дата/список/номер` (номер — среди видимых, с 1). Скрытая — «не найдена».
    fn resolve(&self, reference: &str) -> Result<Found> {
        let hidden = self.hidden()?;
        let reference = reference.trim();
        let parts: Vec<&str> = reference.split('/').collect();
        if let [date, list, number] = parts[..] {
            let date = Date::parse_iso(date).with_context(|| format!("bad date in task ref «{reference}»"))?;
            let category = parse_list(list)?;
            let number: usize = number.parse().with_context(|| format!("bad number in task ref «{reference}»"))?;
            let board = self.book.load(date)?;
            let visible = Self::visible_indices(&board, category, &hidden);
            let index = number
                .checked_sub(1)
                .and_then(|n| visible.get(n).copied())
                .with_context(|| format!("task «{reference}» not found"))?;
            return Ok(Found { date, category, index, board });
        }
        let mut fallback = None;
        for date in self.book.dates()?.into_iter().rev() {
            let board = self.book.load(date)?;
            for category in Category::ALL {
                let found = board.list(category).iter().position(|t| {
                    t.id.as_deref() == Some(reference) && Self::visible(t, category, &hidden)
                });
                let Some(index) = found else { continue };
                let live = !matches!(board.list(category)[index].state, State::Migrated(_));
                let found = Found { date, category, index, board: board.clone() };
                if live {
                    return Ok(found);
                }
                fallback.get_or_insert(found);
            }
        }
        fallback.with_context(|| format!("task «{reference}» not found"))
    }

    /// Даёт задаче id (если ещё нет) и сохраняет страницу — чтобы дальше ссылаться на неё
    /// устойчиво, что бы ни делали с порядком задач.
    fn ensure_id(&self, found: &mut Found) -> Result<String> {
        if let Some(id) = &found.task().id {
            return Ok(id.clone());
        }
        let id = self.book.new_id();
        found.task_mut().id = Some(id.clone());
        self.book.save(found.date, &found.board)?;
        Ok(id)
    }

    fn check_writable(&self, category: Category) -> Result<()> {
        if self.hidden()?[category.index()] {
            // Не раскрываем, что в списке; просто нельзя.
            bail!("list «{}» is not available", category.title());
        }
        Ok(())
    }

    // ------------------------------------------------------------------ чтение

    /// Страницы с видимыми делами за период: дата и сколько открыто / сделано.
    pub fn list_pages(&self, from: Option<Date>, to: Option<Date>) -> Result<String> {
        let hidden = self.hidden()?;
        let mut out = String::new();
        for date in self.book.dates()? {
            if from.is_some_and(|f| date < f) || to.is_some_and(|t| date > t) {
                continue;
            }
            let board = self.book.load(date)?;
            let (mut open, mut done) = (0, 0);
            for category in Category::ALL {
                for i in Self::visible_indices(&board, category, &hidden) {
                    match board.list(category)[i].state {
                        State::Open => open += 1,
                        State::Done => done += 1,
                        State::Migrated(_) => {}
                    }
                }
            }
            if open + done > 0 {
                out.push_str(&format!("{date} ({}): {open} open, {done} done\n", date.format(c"%A")));
            }
        }
        if out.is_empty() {
            out.push_str("No pages with tasks in this period.\n");
        }
        Ok(out)
    }

    /// Страницы за период целиком (только видимое), по желанию — только некоторые списки и
    /// с записями задач.
    pub fn get_pages(&self, from: Date, to: Date, lists: Option<&[Category]>, with_notes: bool) -> Result<String> {
        if to < from {
            bail!("`to` is before `from`");
        }
        if from.days_until(to) > 366 {
            bail!("period is too long (max 366 days)");
        }
        let hidden = self.hidden()?;
        let today = Date::today();
        let mut out = String::new();
        let mut date = from;
        while date <= to {
            let board = self.book.load(date)?;
            let summaries = self.book.summaries(&board);
            let mut page = String::new();
            let mut any = false;
            for category in Category::ALL {
                if hidden[category.index()] || lists.is_some_and(|l| !l.contains(&category)) {
                    continue;
                }
                page.push_str(&format!("### {}\n", category.title()));
                let visible = Self::visible_indices(&board, category, &hidden);
                if visible.is_empty() {
                    page.push_str("(empty)\n");
                }
                any |= !visible.is_empty();
                for (n, &i) in visible.iter().enumerate() {
                    let task = &board.list(category)[i];
                    let notes = task
                        .id
                        .as_ref()
                        .and_then(|id| summaries.get(id))
                        .map(|s| {
                            if s.subtasks > 0 {
                                format!(" [notes, subtasks {}/{}]", s.subtasks_done, s.subtasks)
                            } else {
                                " [notes]".into()
                            }
                        })
                        .unwrap_or_default();
                    page.push_str(&format!(
                        "{}. {} {}{notes} — ref: {}\n",
                        n + 1,
                        state_mark(task.state),
                        task.text,
                        Self::task_ref(date, category, n + 1, task)
                    ));
                    if with_notes && let Some(id) = &task.id {
                        page.push_str(&indent(&self.render_notes(id)?, "   "));
                    }
                }
            }
            // На период — только заполненные дни; одна дата — всегда, даже пустая.
            if any || from == to {
                let when = match today.days_until(date) {
                    0 => " (today)",
                    -1 => " (yesterday)",
                    1 => " (tomorrow)",
                    _ => "",
                };
                out.push_str(&format!("## {date}, {}{when}\n{page}\n", date.format(c"%A")));
            }
            date = date.add_days(1);
        }
        if out.is_empty() {
            out.push_str("No tasks in this period.\n");
        }
        Ok(out)
    }

    /// Записи задачи с номерами пунктов (для update_note / delete_note).
    fn render_notes(&self, id: &str) -> Result<String> {
        let notes = self.book.load_notes(id)?;
        let mut out = String::new();
        for (n, i) in notes.visible().into_iter().enumerate() {
            out.push_str(&format!("#{} {}", n + 1, notes.raw(i)));
        }
        Ok(out)
    }

    /// Задача целиком: где лежит, история по страницам, записи.
    pub fn get_task(&self, reference: &str) -> Result<String> {
        let found = self.resolve(reference)?;
        let task = found.task();
        let mut out = format!("Task: {}\n", task.text);
        out.push_str(&format!(
            "List: {} · Page: {} · State: {}\n",
            found.category.title(),
            found.date,
            state_mark(task.state)
        ));
        let Some(id) = &task.id else {
            out.push_str("Ref: (no id yet — it gets one with the first note or change)\nNo notes.\n");
            return Ok(out);
        };
        out.push_str(&format!("Ref: {id}\n"));
        // История: все страницы, где была эта задача (перенос сохраняет id).
        let mut history = Vec::new();
        for date in self.book.dates()? {
            let board = self.book.load(date)?;
            for category in Category::ALL {
                for t in board.list(category).iter().filter(|t| t.id.as_deref() == Some(id)) {
                    history.push(format!("{date} {} {}", category.title(), state_mark(t.state)));
                }
            }
        }
        if history.len() > 1 {
            out.push_str("History:\n");
            for line in history {
                out.push_str(&format!("  {line}\n"));
            }
        }
        let notes = self.render_notes(id)?;
        if notes.is_empty() {
            out.push_str("No notes.\n");
        } else {
            out.push_str("Notes (#N — entry number for update_note / delete_note):\n");
            out.push_str(&notes);
        }
        Ok(out)
    }

    /// Поиск по тексту задач и их записей (без учёта регистра).
    pub fn search(&self, query: &str, from: Option<Date>, to: Option<Date>) -> Result<String> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            bail!("empty query");
        }
        let hidden = self.hidden()?;
        let mut out = String::new();
        let mut seen_notes = std::collections::HashSet::new();
        for date in self.book.dates()? {
            if from.is_some_and(|f| date < f) || to.is_some_and(|t| date > t) {
                continue;
            }
            let board = self.book.load(date)?;
            for category in Category::ALL {
                for (n, &i) in Self::visible_indices(&board, category, &hidden).iter().enumerate() {
                    let task = &board.list(category)[i];
                    let mut hits = Vec::new();
                    // Записи одни на все копии задачи — ищем в них один раз.
                    if let Some(id) = &task.id
                        && seen_notes.insert(id.clone())
                    {
                        let notes = self.book.load_notes(id)?;
                        for e in notes.visible() {
                            let text = notes.editable_text(e);
                            if text.to_lowercase().contains(&query) {
                                hits.push(text.lines().next().unwrap_or("").to_string());
                            }
                        }
                    }
                    if task.text.to_lowercase().contains(&query) || !hits.is_empty() {
                        out.push_str(&format!(
                            "{date} · {} · {} {} — ref: {}\n",
                            category.title(),
                            state_mark(task.state),
                            task.text,
                            Self::task_ref(date, category, n + 1, task)
                        ));
                        for hit in hits {
                            out.push_str(&format!("    note: {hit}\n"));
                        }
                    }
                }
            }
        }
        if out.is_empty() {
            out.push_str("Nothing found.\n");
        }
        Ok(out)
    }

    // ------------------------------------------------------------------ правка задач

    pub fn add_task(&self, date: Date, category: Category, text: &str) -> Result<String> {
        self.check_writable(category)?;
        let text = text.trim();
        if text.is_empty() || text.contains('\n') {
            bail!("task text must be one non-empty line");
        }
        let mut board = self.book.load(date)?;
        let mut task = Task::new(text);
        let id = self.book.new_id();
        task.id = Some(id.clone());
        board.push(category, task);
        self.book.save(date, &board)?;
        Ok(format!("Added to {} on {date} — ref: {id}\n", category.title()))
    }

    /// Правка задачи: текст, отметка, другой список, место в списке (с 1, среди видимых).
    pub fn update_task(
        &self,
        reference: &str,
        text: Option<&str>,
        done: Option<bool>,
        list: Option<Category>,
        position: Option<usize>,
    ) -> Result<String> {
        let mut found = self.resolve(reference)?;
        let id = self.ensure_id(&mut found)?;
        if let Some(text) = text {
            let text = text.trim();
            if text.is_empty() || text.contains('\n') {
                bail!("task text must be one non-empty line");
            }
            found.task_mut().text = text.to_string();
        }
        if let Some(done) = done {
            let task = found.task_mut();
            task.state = match (task.state, done) {
                (State::Migrated(to), _) => bail!("task was moved to {to}; change it there"),
                (_, true) => State::Done,
                (_, false) => State::Open,
            };
        }
        if let Some(to) = list
            && to != found.category
        {
            self.check_writable(to)?;
            let index = found.board.move_to(found.category, found.index, to).unwrap();
            found.category = to;
            found.index = index;
        }
        if let Some(position) = position {
            let hidden = self.hidden()?;
            let target = position.max(1) - 1;
            // Сдвигаем по соседям, пока не встанет на нужное место среди видимых.
            loop {
                let visible = Self::visible_indices(&found.board, found.category, &hidden);
                let now = visible.iter().position(|&i| i == found.index).unwrap();
                let target = target.min(visible.len() - 1);
                let step = if now > target {
                    found.board.move_up(found.category, found.index)
                } else if now < target {
                    found.board.move_down(found.category, found.index)
                } else {
                    break;
                };
                match step {
                    Some(index) => found.index = index,
                    None => break,
                }
            }
        }
        self.book.save(found.date, &found.board)?;
        Ok(format!("Updated — ref: {id}\n"))
    }

    pub fn postpone_task(&self, reference: &str, to: Date) -> Result<String> {
        let mut found = self.resolve(reference)?;
        if !found.task().is_open() {
            bail!("only open tasks can be moved to another date");
        }
        if to == found.date {
            bail!("the task is already on {to}");
        }
        let id = self.ensure_id(&mut found)?;
        self.book
            .postpone(&mut found.board, found.date, found.category, found.index, to)?;
        Ok(format!("Moved to {to} — ref: {id}\n"))
    }

    /// Удаление задачи; её записи, если других копий нет, уходят в `DELETED_NOTES.md`.
    pub fn delete_task(&self, reference: &str) -> Result<String> {
        let mut found = self.resolve(reference)?;
        let task = found.board.remove(found.category, found.index).unwrap();
        self.book.save(found.date, &found.board)?;
        let archived = self.book.archive_notes(&task, found.date, &clock::stamp())?;
        Ok(if archived {
            format!("Deleted «{}»; its notes were saved to DELETED_NOTES.md\n", task.text)
        } else {
            format!("Deleted «{}»\n", task.text)
        })
    }

    // ------------------------------------------------------------------ правка записей

    fn notes_of(&self, reference: &str) -> Result<(Found, String, Notes)> {
        let mut found = self.resolve(reference)?;
        let id = self.ensure_id(&mut found)?;
        let notes = self.book.load_notes(&id)?;
        Ok((found, id, notes))
    }

    /// Номер пункта (с 1) — в индекс пункта записей.
    fn entry_index(notes: &Notes, entry: usize) -> Result<usize> {
        entry
            .checked_sub(1)
            .and_then(|n| notes.visible().get(n).copied())
            .with_context(|| format!("no entry #{entry}"))
    }

    /// Заметка (метка времени ставится сама, может быть в несколько строк) или подзадача.
    pub fn add_note(&self, reference: &str, text: &str, subtask: bool) -> Result<String> {
        let text = text.trim_end().trim_start_matches('\n');
        if text.trim().is_empty() {
            bail!("empty text");
        }
        let (_, id, mut notes) = self.notes_of(reference)?;
        if subtask {
            if text.contains('\n') {
                bail!("a subtask is one line; use a note for longer text");
            }
            notes.add_subtask(text.trim());
        } else {
            notes.add_note(&clock::stamp(), text);
        }
        self.book.save_notes(&id, &notes)?;
        Ok(format!("Added entry #{} — ref: {id}\n", notes.visible().len()))
    }

    pub fn update_note(&self, reference: &str, entry: usize, text: Option<&str>, done: Option<bool>) -> Result<String> {
        let (_, id, mut notes) = self.notes_of(reference)?;
        let i = Self::entry_index(&notes, entry)?;
        if let Some(text) = text {
            let text = text.trim_end().trim_start_matches('\n');
            if text.trim().is_empty() {
                bail!("empty text; use delete_note to remove an entry");
            }
            if !notes.is_multiline(i) && text.contains('\n') {
                bail!("a subtask is one line");
            }
            notes.set_text(i, text);
        }
        if let Some(done) = done {
            match notes.item(i).kind {
                Kind::Subtask { done: now } if now != done => {
                    notes.toggle(i);
                }
                Kind::Subtask { .. } => {}
                _ => bail!("entry #{entry} is not a subtask"),
            }
        }
        self.book.save_notes(&id, &notes)?;
        Ok(format!("Updated entry #{entry} — ref: {id}\n"))
    }

    /// Удаление пункта записей; он уходит в `DELETED_NOTES.md`.
    pub fn delete_note(&self, reference: &str, entry: usize) -> Result<String> {
        let (found, id, mut notes) = self.notes_of(reference)?;
        let i = Self::entry_index(&notes, entry)?;
        self.book
            .archive_entry(&found.task().text, &id, found.date, &clock::stamp(), &notes.raw(i))?;
        notes.remove(i);
        self.book.save_notes(&id, &notes)?;
        Ok(format!("Deleted entry #{entry} (saved to DELETED_NOTES.md) — ref: {id}\n"))
    }
}

fn indent(text: &str, prefix: &str) -> String {
    text.lines().map(|l| format!("{prefix}{l}\n")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn d(day: u32) -> Date {
        Date::new(2026, 10, day).unwrap()
    }

    fn agent(name: &str) -> Agent {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-agent-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Agent::new(Book::new(dir))
    }

    fn cleanup(agent: Agent) {
        let _ = fs::remove_dir_all(agent.book().dir());
    }

    /// Страница: работа — открытая, скрытая, сделанная; личное — скрытым списком.
    fn sample(agent: &Agent) {
        let mut board = Board::default();
        board.push(Category::Work, Task::new("отчёт"));
        let mut secret = Task::new("секрет");
        secret.private = true;
        board.push(Category::Work, secret);
        let mut done = Task::new("созвон");
        done.state = State::Done;
        board.push(Category::Work, done);
        board.push(Category::Personal, Task::new("личное дело"));
        agent.book().save(d(26), &board).unwrap();
        agent.book().set_hidden([false, false, true, false]).unwrap();
    }

    #[test]
    fn hidden_is_invisible() {
        let agent = agent("hidden");
        sample(&agent);
        let page = agent.get_pages(d(26), d(26), None, false).unwrap();
        assert!(page.contains("1. [ ] отчёт — ref: 2026-10-26/Работа/1"), "{page}");
        // Нумерация — среди видимых: «созвон» второй, без пропуска.
        assert!(page.contains("2. [x] созвон — ref: 2026-10-26/Работа/2"), "{page}");
        assert!(!page.contains("секрет") && !page.contains("Личное") && !page.contains("личное дело"), "{page}");
        assert_eq!(agent.search("секрет", None, None).unwrap(), "Nothing found.\n");
        assert_eq!(agent.search("личное", None, None).unwrap(), "Nothing found.\n");
        assert!(agent.add_task(d(26), Category::Personal, "x").is_err());
        assert!(agent.update_task("2026-10-26/Работа/1", None, None, Some(Category::Personal), None).is_err());
        assert!(agent.get_task("2026-10-26/Работа/3").is_err());
        assert!(agent.list_pages(None, None).unwrap().contains("2026-10-26 (Monday): 1 open, 1 done"));
        cleanup(agent);
    }

    #[test]
    fn tasks_and_notes() {
        let agent = agent("tasks");
        sample(&agent);
        // Ссылка по номеру → id с первой правки.
        let out = agent.add_note("2026-10-26/work/1", "начал\nпродолжение", false).unwrap();
        let id = out.rsplit("ref: ").next().unwrap().trim().to_string();
        assert_eq!(id.len(), 6);
        agent.add_note(&id, "собрать цифры", true).unwrap();
        agent.update_note(&id, 2, None, Some(true)).unwrap();
        let task = agent.get_task(&id).unwrap();
        assert!(task.contains("начал\n  продолжение"), "{task}");
        assert!(task.contains("#2 - [x] собрать цифры"), "{task}");
        assert!(agent.update_note(&id, 1, None, Some(true)).is_err());

        // Скрытая задача не сдвинулась: «отчёт» на второе место среди видимых.
        agent.update_task(&id, Some("отчёт за квартал"), None, None, Some(2)).unwrap();
        let board = agent.book().load(d(26)).unwrap();
        let work: Vec<&str> = board.list(Category::Work).iter().map(|t| t.text.as_str()).collect();
        assert_eq!(work, ["секрет", "созвон", "отчёт за квартал"]);

        // Перенос: ссылка по id ведёт на живую копию.
        agent.postpone_task(&id, d(27)).unwrap();
        let task = agent.get_task(&id).unwrap();
        assert!(task.contains("Page: 2026-10-27 · State: [ ]"), "{task}");
        assert!(task.contains("2026-10-26 Работа [>] (moved to 2026-10-27)"), "{task}");
        agent.update_task(&id, None, Some(true), None, None).unwrap();
        assert!(agent.search("цифры", None, None).unwrap().contains("note: собрать цифры"));

        // Удаление пункта и задачи — в архив.
        agent.delete_note(&id, 1).unwrap();
        let deleted = fs::read_to_string(agent.book().dir().join(crate::book::DELETED_NOTES)).unwrap();
        assert!(deleted.contains("- 2026-") && deleted.contains("  продолжение\n"), "{deleted}");
        agent.delete_task(&format!("{}/Работа/1", d(27))).unwrap();
        // Копия-запись на 26-м ещё есть — записи живы.
        assert!(agent.get_task(&id).unwrap().contains("собрать цифры"));
        assert!(agent.add_task(d(28), Category::Home, "две\nстроки").is_err());
        let out = agent.add_task(d(28), Category::Home, "новая").unwrap();
        assert!(out.starts_with("Added to Дом on 2026-10-28 — ref: "), "{out}");
        cleanup(agent);
    }

    #[test]
    fn dates_and_lists() {
        let today = d(26);
        assert_eq!(parse_date("today", today).unwrap(), today);
        assert_eq!(parse_date("Завтра", today).unwrap(), d(27));
        assert_eq!(parse_date("+3", today).unwrap(), d(29));
        assert_eq!(parse_date("2026-10-01", today).unwrap(), d(1));
        assert!(parse_date("потом", today).is_err());
        assert_eq!(parse_list("WORK").unwrap(), Category::Work);
        assert_eq!(parse_list("личное").unwrap(), Category::Personal);
        assert!(parse_list("отпуск").is_err());
    }
}
