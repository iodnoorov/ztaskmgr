use super::*;
use anyhow::{Context, Result, ensure};
use ztaskmgr_core::notebook::{self, Page};

#[derive(Debug, Default)]
pub struct NotebookView {
    pub group: Option<String>,
    pub row: usize,
    pub linked: bool,
    pub query: String,
    pub input: Option<(char, LineInput)>,
    pub reading: Option<Page>,
    pub scroll: u16,
    pub moving: Option<String>,
    pub confirm: bool,
}
pub struct NotebookEditor {
    pub base: Page,
    pub path: PathBuf,
    pub relative: String,
}
#[derive(Clone)]
pub struct NotebookRow {
    pub id: String,
    pub title: String,
    pub group: bool,
    pub hidden: bool,
    pub linked: bool,
}
impl App {
    pub fn notebook_rows(&self, view: &NotebookView) -> Result<Vec<NotebookRow>> {
        let groups = self.book.groups()?;
        let mut pages = self.book.notebook_pages()?;
        pages.sort_by(|a, b| (a.title(), &a.id).cmp(&(b.title(), &b.id)));
        let refs = self.selected_task().map(Task::references).unwrap_or_default();
        let query = view.query.to_lowercase();
        let mut rows = Vec::new();
        if !view.linked {
            for g in groups.iter().filter(|g| {
                !g.deleted
                    && (view.moving.is_some()
                        || (query.is_empty()
                            && (g.parent == view.group
                                || (view.group.is_none()
                                    && (g.parent.as_ref().is_some_and(|p| {
                                        !groups.iter().any(|parent| &parent.id == p && !parent.deleted)
                                    }) || notebook::descendant(&groups, g.parent.as_deref(), &g.id))))))
            }) {
                rows.push(NotebookRow {
                    id: g.id.clone(),
                    title: g.title.clone(),
                    group: true,
                    hidden: notebook::group_hidden(&groups, Some(&g.id)),
                    linked: false,
                });
            }
        }
        if view.moving.is_none() {
            for p in pages.iter().filter(|p| !p.deleted) {
                let linked = refs.contains(&p.id.as_str());
                let known_group =
                    p.group.as_ref().filter(|id| groups.iter().any(|g| &g.id == *id && !g.deleted)).cloned();
                if (view.linked && linked)
                    || (!view.linked
                        && if query.is_empty() {
                            known_group == view.group
                        } else {
                            p.body.to_lowercase().contains(&query)
                        })
                {
                    rows.push(NotebookRow {
                        id: p.id.clone(),
                        title: format!("{}{}", p.title(), if p.conflicted() { " [конфликт]" } else { "" }),
                        group: false,
                        hidden: p.hidden || notebook::group_hidden(&groups, p.group.as_deref()),
                        linked,
                    });
                }
            }
            if view.linked {
                for id in refs {
                    if !pages.iter().any(|p| p.id == id && !p.deleted) {
                        rows.push(NotebookRow {
                            id: id.into(),
                            title: "Заметка удалена / недоступна".into(),
                            group: false,
                            hidden: false,
                            linked: true,
                        });
                    }
                }
            }
        }
        Ok(rows)
    }
    pub fn notebook_location(&self, view: &NotebookView) -> String {
        if view.moving.is_some() {
            return "Выберите группу: Enter — сюда, Home — без группы, Esc — отмена".into();
        }
        if view.linked {
            return "Заметки задачи".into();
        }
        let groups = self.book.groups().unwrap_or_default();
        let mut labels = Vec::new();
        let mut current = view.group.as_deref();
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id) {
                break;
            }
            let Some(g) = groups.iter().find(|g| g.id == id) else {
                break;
            };
            labels.push(g.title.clone());
            current = g.parent.as_deref();
        }
        labels.reverse();
        format!("Заметки / {}", labels.join(" / "))
    }
    pub(super) fn on_key_notebook(&mut self, press: Press) {
        let Some(mut view) = self.notebook.take() else {
            return;
        };
        let result = self.notebook_key(&mut view, &press);
        match result {
            Ok(false) => return,
            Err(e) => self.status = Some(Status::Error(format!("Заметки: {e:#}"))),
            _ => {}
        }
        self.notebook = Some(view);
    }
    fn notebook_key(&mut self, v: &mut NotebookView, press: &Press) -> Result<bool> {
        if let Some((kind, input)) = &mut v.input {
            match edit_line(input, press) {
                Some(KeyCode::Esc) => {
                    v.input = None;
                }
                Some(KeyCode::Enter) => {
                    let kind = *kind;
                    let value = input.text().to_string();
                    let rows = self.notebook_rows(v)?;
                    let selected = rows.get(v.row);
                    match kind {
                        '/' => v.query = value,
                        'n' => {
                            let id = self.book.create_notebook_page(&value, v.group.as_deref())?;
                            self.start_notebook_editor(&id)?;
                        }
                        'g' => {
                            self.book.create_group(&value, v.group.as_deref())?;
                        }
                        'r' => {
                            if let Some(row) = selected {
                                let groups = self.book.groups()?;
                                let g = groups.iter().find(|g| g.id == row.id).context("группа не найдена")?;
                                self.book.update_group(&g.id, &value, g.parent.as_deref(), false)?;
                            }
                        }
                        _ => {}
                    }
                    v.input = None;
                    v.row = 0;
                    self.edited = true;
                }
                _ => {}
            }
            return Ok(true);
        }
        if let Some(page) = &v.reading {
            match press.hotkey.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => {
                    v.reading = None;
                    v.scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => v.scroll = v.scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => v.scroll = v.scroll.saturating_sub(1),
                KeyCode::PageDown => v.scroll = v.scroll.saturating_add(10),
                KeyCode::PageUp => v.scroll = v.scroll.saturating_sub(10),
                KeyCode::Char('e') => self.start_notebook_editor(&page.id)?,
                _ => {}
            }
            return Ok(true);
        }
        let rows = self.notebook_rows(v)?;
        v.row = v.row.min(rows.len().saturating_sub(1));
        let row = rows.get(v.row);
        if v.confirm {
            if press.hotkey.code == KeyCode::Char('y')
                && let Some(row) = row
            {
                if row.group {
                    let groups = self.book.groups()?;
                    let g = groups.iter().find(|g| g.id == row.id).context("группа не найдена")?;
                    self.book.update_group(&g.id, &g.title, g.parent.as_deref(), true)?;
                } else {
                    self.book.update_notebook_page(&row.id, |p| {
                        p.deleted = true;
                        Ok(())
                    })?;
                }
                self.edited = true;
            }
            v.confirm = false;
            return Ok(true);
        }
        if let Some(source) = v.moving.clone() {
            match press.hotkey.code {
                KeyCode::Esc => v.moving = None,
                KeyCode::Down | KeyCode::Char('j') => v.row = (v.row + 1).min(rows.len().saturating_sub(1)),
                KeyCode::Up | KeyCode::Char('k') => v.row = v.row.saturating_sub(1),
                KeyCode::Enter | KeyCode::Home => {
                    let target = if press.hotkey.code == KeyCode::Home {
                        None
                    } else {
                        Some(row.context("выберите группу")?.id.as_str())
                    };
                    if let Some(id) = source.strip_prefix("group:") {
                        let groups = self.book.groups()?;
                        let g = groups.iter().find(|g| g.id == id).context("группа не найдена")?;
                        self.book.update_group(id, &g.title, target, false)?;
                    } else {
                        self.book.update_notebook_page(&source, |p| {
                            p.group = target.map(str::to_string);
                            Ok(())
                        })?;
                    }
                    v.moving = None;
                    v.row = 0;
                    self.edited = true;
                }
                _ => {}
            }
            return Ok(true);
        }
        match press.hotkey.code {
            KeyCode::Esc | KeyCode::Char('q' | 'N') => return Ok(false),
            KeyCode::Backspace => {
                v.group =
                    self.book.groups()?.iter().find(|g| Some(&g.id) == v.group.as_ref()).and_then(|g| g.parent.clone());
                v.query.clear();
                v.row = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => v.row = (v.row + 1).min(rows.len().saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => v.row = v.row.saturating_sub(1),
            KeyCode::Tab => {
                v.linked = !v.linked;
                v.row = 0;
            }
            KeyCode::Char('n' | 'g' | '/') => {
                let KeyCode::Char(kind) = press.hotkey.code else { unreachable!() };
                v.input = Some((kind, LineInput::new("")));
            }
            KeyCode::Char('r') => {
                if let Some(row) = row.filter(|r| r.group) {
                    v.input = Some(('r', LineInput::new(&row.title)));
                }
            }
            KeyCode::Enter => {
                if let Some(row) = row {
                    if row.group {
                        v.group = Some(row.id.clone());
                        v.row = 0;
                    } else {
                        let p = self.book.notebook_page(&row.id)?;
                        ensure!(!p.deleted, "заметка удалена");
                        v.reading = Some(p);
                    }
                }
            }
            KeyCode::Char('e') => {
                if let Some(row) = row.filter(|r| !r.group) {
                    self.start_notebook_editor(&row.id)?;
                }
            }
            KeyCode::Char('m') => {
                if let Some(row) = row {
                    v.moving = Some(if row.group { format!("group:{}", row.id) } else { row.id.clone() });
                    v.row = 0;
                }
            }
            KeyCode::Char('p') => {
                if let Some(row) = row {
                    if row.group {
                        self.book.set_group_hidden(&row.id, !row.hidden)?;
                    } else {
                        let groups = self.book.groups()?;
                        self.book.update_notebook_page(&row.id, |p| {
                            ensure!(p.hidden == row.hidden, "родитель скрыт");
                            ensure!(
                                !p.hidden || !notebook::group_hidden(&groups, p.group.as_deref()),
                                "родитель скрыт"
                            );
                            p.hidden = !p.hidden;
                            Ok(())
                        })?;
                    }
                    self.edited = true;
                }
            }
            KeyCode::Char('a') => {
                if let Some(row) = row.filter(|r| !r.group) {
                    ensure!(self.editable(), "страница недоступна");
                    let i = self.selected().context("выберите задачу")?;
                    let task = self.board.get_mut(self.active, i).context("выберите задачу")?;
                    let mut ids: Vec<String> = task.references().into_iter().map(str::to_string).collect();
                    if ids.contains(&row.id) {
                        ids.retain(|id| id != &row.id);
                    } else {
                        ensure!(!self.book.notebook_page(&row.id)?.deleted, "заметка удалена");
                        ids.push(row.id.clone());
                    }
                    task.set_references(&ids);
                    self.save();
                }
            }
            KeyCode::Char('d') => {
                if let Some(row) = row {
                    let mut count = 0;
                    if !row.group {
                        for date in self.book.dates()? {
                            count += self
                                .book
                                .load(date)?
                                .tasks()
                                .filter(|t| t.references().contains(&row.id.as_str()))
                                .count();
                        }
                    }
                    self.status = Some(Status::Info(format!(
                        "Удалить «{}»? Ссылок на страницах: {count}. Содержимое группы будет перенесено на верхний уровень. y — удалить, другая клавиша — отмена",
                        row.title
                    )));
                    v.confirm = true;
                }
            }
            _ => {}
        }
        Ok(true)
    }
    fn start_notebook_editor(&mut self, id: &str) -> Result<()> {
        ensure!(self.notebook_editor.is_none(), "сначала завершите предыдущую правку");
        let base = self.book.notebook_page(id)?;
        ensure!(!base.deleted, "заметка удалена");
        let relative = format!(".note-edit-{}-{}-{}.md", id, std::process::id(), clock::now_ms());
        self.book.write_file(&relative, base.body.as_bytes())?;
        let path = self.book.dir().join(&relative);
        self.editor_request = Some(path.clone());
        self.notebook_editor = Some(NotebookEditor { base, path, relative });
        Ok(())
    }
    pub(super) fn finish_notebook_editor(&mut self, error: Option<String>) {
        let Some(draft) = self.notebook_editor.take() else {
            return;
        };
        let result = (|| -> Result<()> {
            if let Some(error) = error {
                anyhow::bail!(error);
            }
            let body = std::fs::read_to_string(&draft.path)?;
            if body.trim() == draft.base.body.trim() {
                self.book.remove_file(&draft.relative)?;
                return Ok(());
            }
            self.book.update_notebook_page(&draft.base.id, |current| {
                let mut edited = current.clone();
                edited.body = body.trim().into();
                if current.body != draft.base.body {
                    edited = Page::parse(&notebook::merge_pages(&current.serialize(), &edited.serialize())?)?;
                }
                current.body = edited.body;
                Ok(())
            })?;
            self.book.remove_file(&draft.relative)?;
            if let Some(view) = &mut self.notebook
                && view.reading.as_ref().is_some_and(|p| p.id == draft.base.id)
            {
                view.reading = Some(self.book.notebook_page(&draft.base.id)?);
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.status = Some(Status::Error(format!("{e:#}. Черновик: {}", draft.path.display())));
        }
        self.edited = true;
    }
}
