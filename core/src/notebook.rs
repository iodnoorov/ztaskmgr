//! Самостоятельные Markdown-заметки и группы. Все изменения — под блокировкой книжки.
use std::collections::{BTreeMap, BTreeSet};

use crate::{book::Book, clock, storage};
use anyhow::{Context, Result, bail, ensure};

pub const GROUPS_FILE: &str = "GROUPS.md";
const VERSION: &str = "\n\n<!-- ztaskmgr conflict version -->\n\n";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub parent: Option<String>,
    pub title: String,
    pub hidden: bool,
    pub deleted: bool,
    pub modified: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub id: String,
    pub group: Option<String>,
    pub hidden: bool,
    pub deleted: bool,
    pub modified: u64,
    pub body: String,
}

fn meta(line: &str) -> Result<BTreeMap<&str, &str>> {
    let (_, tail) = line.split_once("<!-- ").context("нет метаданных")?;
    let tail = tail.strip_suffix(" -->").context("незавершённые метаданные")?;
    let mut out = BTreeMap::new();
    for field in tail.split_whitespace() {
        let (key, value) = field.split_once(':').context("неизвестное поле метаданных")?;
        ensure!(out.insert(key, value).is_none(), "повтор поля {key}");
    }
    Ok(out)
}
fn identity(value: &str) -> Result<String> {
    ensure!(storage::is_valid_id(value), "некорректный ID");
    Ok(value.into())
}
fn parent(value: Option<&&str>) -> Result<Option<String>> {
    match value.copied().unwrap_or("root") {
        "root" => Ok(None),
        id => identity(id).map(Some),
    }
}
fn flag(fields: &BTreeMap<&str, &str>, name: &str) -> Result<bool> {
    match fields.get(name).copied().unwrap_or("0") {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => bail!("неверный флаг {name}"),
    }
}
fn time(fields: &BTreeMap<&str, &str>) -> Result<u64> {
    fields.get("t").unwrap_or(&"0").parse().context("неверное время правки")
}
fn title_valid(title: &str) -> Result<()> {
    ensure!(
        !title.trim().is_empty() && !title.contains(['\n', '\r']) && !title.contains("<!--"),
        "некорректное название"
    );
    Ok(())
}

pub fn parse_groups(text: &str) -> Result<Vec<Group>> {
    let mut groups = Vec::new();
    let mut ids = BTreeSet::new();
    for line in text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
        let fields = meta(line)?;
        ensure!(
            fields.keys().all(|k| ["group", "parent", "hidden", "deleted", "t"].contains(k)),
            "неизвестное поле группы"
        );
        let id = identity(fields.get("group").context("нет ID группы")?)?;
        ensure!(ids.insert(id.clone()), "повтор ID группы");
        let title = line
            .strip_prefix("- ")
            .context("неверная строка группы")?
            .split_once(" <!-- ")
            .context("нет названия")?
            .0
            .to_string();
        title_valid(&title)?;
        groups.push(Group {
            id,
            parent: parent(fields.get("parent"))?,
            title,
            hidden: flag(&fields, "hidden")?,
            deleted: flag(&fields, "deleted")?,
            modified: time(&fields)?,
        });
    }
    Ok(groups)
}
pub fn serialize_groups(groups: &[Group]) -> String {
    let mut out = String::from("# Группы заметок\n\n");
    for g in groups {
        out.push_str(&format!(
            "- {} <!-- group:{} parent:{} hidden:{} deleted:{} t:{} -->\n",
            g.title,
            g.id,
            g.parent.as_deref().unwrap_or("root"),
            u8::from(g.hidden),
            u8::from(g.deleted),
            g.modified
        ));
    }
    out
}
impl Page {
    pub fn parse(text: &str) -> Result<Self> {
        let mut lines = text.splitn(3, '\n');
        ensure!(lines.next() == Some("<!-- ztaskmgr note 1 -->"), "неизвестный формат заметки");
        let m = meta(lines.next().context("нет метаданных заметки")?)?;
        ensure!(m.keys().all(|k| ["id", "group", "hidden", "deleted", "t"].contains(k)), "неизвестное поле заметки");
        let page = Self {
            id: identity(m.get("id").context("нет ID заметки")?)?,
            group: parent(m.get("group"))?,
            hidden: flag(&m, "hidden")?,
            deleted: flag(&m, "deleted")?,
            modified: time(&m)?,
            body: lines.next().unwrap_or("").trim().to_string(),
        };
        ensure!(page.body.lines().any(|l| l.starts_with("# ") && l.len() > 2), "нужен заголовок # Название");
        Ok(page)
    }
    pub fn serialize(&self) -> String {
        format!(
            "<!-- ztaskmgr note 1 -->\n<!-- id:{} group:{} hidden:{} deleted:{} t:{} -->\n\n{}\n",
            self.id,
            self.group.as_deref().unwrap_or("root"),
            u8::from(self.hidden),
            u8::from(self.deleted),
            self.modified,
            self.body.trim()
        )
    }
    pub fn title(&self) -> &str {
        self.body.lines().find_map(|l| l.strip_prefix("# ")).unwrap_or("Без названия")
    }
    pub fn conflicted(&self) -> bool {
        self.body.contains(VERSION)
    }
}

/// Цикл, потерянный или удалённый родитель закрывает доступ для ИИ.
pub fn group_hidden(groups: &[Group], id: Option<&str>) -> bool {
    let mut current = id;
    let mut seen = BTreeSet::new();
    while let Some(id) = current {
        if !seen.insert(id) {
            return true;
        }
        let Some(g) = groups.iter().find(|g| g.id == id) else {
            return true;
        };
        if g.hidden || g.deleted {
            return true;
        }
        current = g.parent.as_deref();
    }
    false
}
pub fn descendant(groups: &[Group], child: Option<&str>, ancestor: &str) -> bool {
    let mut current = child;
    let mut seen = BTreeSet::new();
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        if !seen.insert(id) {
            break;
        }
        current = groups.iter().find(|g| g.id == id).and_then(|g| g.parent.as_deref());
    }
    false
}
pub fn merge_groups(ours: &str, theirs: &str) -> Result<String> {
    let mut groups = parse_groups(ours)?;
    for g in parse_groups(theirs)? {
        if let Some(mine) = groups.iter_mut().find(|m| m.id == g.id) {
            // Скрытие и удаление при конфликте безопаснее открытия и восстановления.
            let hidden = mine.hidden || g.hidden;
            let deleted = mine.deleted || g.deleted;
            if (g.modified, &g.title, &g.parent) > (mine.modified, &mine.title, &mine.parent) {
                *mine = g;
            }
            mine.hidden = hidden;
            mine.deleted = deleted;
        } else {
            groups.push(g);
        }
    }
    Ok(serialize_groups(&groups))
}
pub fn merge_pages(ours: &str, theirs: &str) -> Result<String> {
    if ours.is_empty() {
        return Ok(Page::parse(theirs)?.serialize());
    }
    let a = Page::parse(ours)?;
    let b = Page::parse(theirs)?;
    ensure!(a.id == b.id, "несовпадение ID заметок");
    let mut merged = if (a.modified, &a.group) >= (b.modified, &b.group) { a.clone() } else { b.clone() };
    merged.hidden = a.hidden || b.hidden;
    merged.deleted = a.deleted || b.deleted;
    let versions: BTreeSet<_> = a.body.split(VERSION).chain(b.body.split(VERSION)).map(str::trim).collect();
    merged.body = versions.into_iter().collect::<Vec<_>>().join(VERSION);
    Ok(merged.serialize())
}

impl Book {
    pub fn groups(&self) -> Result<Vec<Group>> {
        let text = self.read_file(GROUPS_FILE)?.map(String::from_utf8).transpose()?;
        parse_groups(text.as_deref().unwrap_or(""))
    }
    pub fn notebook_pages(&self) -> Result<Vec<Page>> {
        self.files()?
            .into_iter()
            .filter_map(|path| path.strip_prefix("pages/").map(str::to_string))
            .map(|name| self.notebook_page(name.trim_end_matches(".md")))
            .collect()
    }
    pub fn notebook_page(&self, id: &str) -> Result<Page> {
        identity(id)?;
        let text = String::from_utf8(self.read_file(&format!("pages/{id}.md"))?.context("заметка не найдена")?)?;
        let page = Page::parse(&text).with_context(|| format!("pages/{id}.md"))?;
        ensure!(page.id == id, "ID заметки не совпадает с именем файла");
        Ok(page)
    }
    fn write_notebook_page(&self, page: &Page) -> Result<()> {
        self.write_file(&format!("pages/{}.md", page.id), page.serialize().as_bytes())
    }
    fn notebook_id(&self) -> Result<String> {
        loop {
            let id = self.new_id()?;
            if self.groups()?.iter().all(|g| g.id != id) && self.read_file(&format!("pages/{id}.md"))?.is_none() {
                return Ok(id);
            }
        }
    }
    pub fn create_group(&self, title: &str, parent: Option<&str>) -> Result<String> {
        let _guard = self.lock()?;
        title_valid(title)?;
        let mut groups = self.groups()?;
        if let Some(id) = parent {
            ensure!(groups.iter().any(|g| g.id == id && !g.deleted), "группа не найдена");
        }
        let id = self.notebook_id()?;
        let hidden = group_hidden(&groups, parent);
        groups.push(Group {
            id: id.clone(),
            parent: parent.map(str::to_string),
            title: title.trim().into(),
            hidden,
            deleted: false,
            modified: clock::now_ms(),
        });
        self.write_file(GROUPS_FILE, serialize_groups(&groups).as_bytes())?;
        Ok(id)
    }
    pub fn create_notebook_page(&self, title: &str, group: Option<&str>) -> Result<String> {
        let _guard = self.lock()?;
        title_valid(title)?;
        let groups = self.groups()?;
        if let Some(id) = group {
            ensure!(groups.iter().any(|g| g.id == id && !g.deleted), "группа не найдена");
        }
        let id = self.notebook_id()?;
        self.write_notebook_page(&Page {
            id: id.clone(),
            group: group.map(str::to_string),
            hidden: group_hidden(&groups, group),
            deleted: false,
            modified: clock::now_ms(),
            body: format!("# {}", title.trim()),
        })?;
        Ok(id)
    }
    /// Перед открытием родителя материализуем скрытие всех детей, в том числе пришедших с другого устройства.
    pub fn set_group_hidden(&self, id: &str, hidden: bool) -> Result<()> {
        let _guard = self.lock()?;
        let mut groups = self.groups()?;
        let g = groups.iter().find(|g| g.id == id && !g.deleted).context("группа не найдена")?;
        ensure!(hidden || !group_hidden(&groups, g.parent.as_deref()), "родитель скрыт");
        if hidden || g.hidden {
            // Сначала закрываем родителя: частичный сбой не откроет доступ.
            let i = groups.iter().position(|g| g.id == id).unwrap();
            groups[i].hidden = true;
            groups[i].modified = clock::now_ms().max(groups[i].modified.saturating_add(1));
            self.write_file(GROUPS_FILE, serialize_groups(&groups).as_bytes())?;
            for mut page in self.notebook_pages()? {
                if descendant(&groups, page.group.as_deref(), id) && !page.hidden {
                    page.hidden = true;
                    page.modified = clock::now_ms().max(page.modified.saturating_add(1));
                    self.write_notebook_page(&page)?;
                }
            }
            let children: Vec<_> =
                groups.iter().filter(|g| descendant(&groups, Some(&g.id), id)).map(|g| g.id.clone()).collect();
            for g in &mut groups {
                if children.contains(&g.id) {
                    g.hidden = true;
                    g.modified = clock::now_ms().max(g.modified.saturating_add(1));
                }
            }
        }
        let g = groups.iter_mut().find(|g| g.id == id).unwrap();
        g.hidden = hidden;
        g.modified = clock::now_ms().max(g.modified.saturating_add(1));
        self.write_file(GROUPS_FILE, serialize_groups(&groups).as_bytes())
    }
    pub fn update_notebook_page(&self, id: &str, edit: impl FnOnce(&mut Page) -> Result<()>) -> Result<()> {
        let _guard = self.lock()?;
        let mut page = self.notebook_page(id)?;
        let old = page.clone();
        ensure!(!page.deleted, "заметка удалена");
        edit(&mut page)?;
        ensure!(page.id == old.id, "нельзя менять ID");
        let groups = self.groups()?;
        if let Some(id) = &page.group {
            ensure!(groups.iter().any(|g| &g.id == id && !g.deleted), "группа не найдена");
        }
        if group_hidden(&groups, old.group.as_deref()) && page.group != old.group {
            page.hidden = true;
        }
        if group_hidden(&groups, page.group.as_deref()) {
            page.hidden = true;
        }
        page.modified = clock::now_ms().max(old.modified.saturating_add(1));
        Page::parse(&page.serialize())?;
        self.write_notebook_page(&page)
    }
    pub fn update_group(&self, id: &str, title: &str, parent: Option<&str>, deleted: bool) -> Result<()> {
        let _guard = self.lock()?;
        title_valid(title)?;
        let mut groups = self.groups()?;
        if let Some(p) = parent {
            ensure!(groups.iter().any(|g| g.id == p && !g.deleted), "группа не найдена");
            ensure!(!descendant(&groups, Some(p), id), "цикл групп");
        }
        let inherited = group_hidden(&groups, parent);
        if group_hidden(&groups, Some(id)) {
            self.set_group_hidden(id, true)?;
            groups = self.groups()?;
        }
        let g = groups.iter_mut().find(|g| g.id == id && !g.deleted).context("группа не найдена")?;
        g.title = title.trim().into();
        g.parent = parent.map(str::to_string);
        g.modified = clock::now_ms().max(g.modified.saturating_add(1));
        if deleted {
            // Содержимое переносится на верхний уровень, с сохранением скрытия.
            for mut page in self.notebook_pages()? {
                if page.group.as_deref() == Some(id) {
                    page.group = None;
                    page.modified = clock::now_ms().max(page.modified.saturating_add(1));
                    self.write_notebook_page(&page)?;
                }
            }
            for child in &mut groups {
                if child.parent.as_deref() == Some(id) {
                    child.parent = None;
                    child.modified = clock::now_ms().max(child.modified.saturating_add(1));
                }
            }
            groups.iter_mut().find(|g| g.id == id).unwrap().deleted = true;
        } else if inherited {
            g.hidden = true;
        }
        self.write_file(GROUPS_FILE, serialize_groups(&groups).as_bytes())?;
        if inherited && !deleted {
            self.set_group_hidden(id, true)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lists::Lists, merge, model::Task, store::MemStore};
    fn book() -> Book {
        Book::with_store(std::path::PathBuf::from("/unused-notebook"), Box::new(MemStore::default()))
    }
    #[test]
    fn nested_privacy_is_sticky_including_late_arrivals() {
        let b = book();
        let root = b.create_group("Программирование", None).unwrap();
        let child = b.create_group("Языки", Some(&root)).unwrap();
        let note = b.create_notebook_page("Rust", Some(&child)).unwrap();
        b.set_group_hidden(&root, true).unwrap();
        assert!(b.notebook_page(&note).unwrap().hidden);
        assert!(b.groups().unwrap().iter().all(|g| g.hidden));
        assert!(b.set_group_hidden(&child, false).is_err());
        let fresh = b.create_notebook_page("PHP", Some(&root)).unwrap();
        assert!(b.notebook_page(&fresh).unwrap().hidden);
        // Заметка пришла с другого устройства без собственного скрытия.
        b.write_file(
            "pages/late.md",
            Page {
                id: "late".into(),
                group: Some(root.clone()),
                hidden: false,
                deleted: false,
                modified: 0,
                body: "# Поздняя".into(),
            }
            .serialize()
            .as_bytes(),
        )
        .unwrap();
        b.set_group_hidden(&root, false).unwrap();
        assert!(b.notebook_page("late").unwrap().hidden);
        assert!(b.groups().unwrap().iter().find(|g| g.id == child).unwrap().hidden);
        b.set_group_hidden(&child, false).unwrap();
        assert!(b.notebook_page(&note).unwrap().hidden);
        b.update_notebook_page(&note, |p| {
            p.hidden = false;
            Ok(())
        })
        .unwrap();
        assert!(!b.notebook_page(&note).unwrap().hidden);
    }
    #[test]
    fn moving_out_of_hidden_parent_keeps_privacy_and_cycles_are_rejected() {
        let b = book();
        let a = b.create_group("A", None).unwrap();
        let c = b.create_group("C", Some(&a)).unwrap();
        let p = b.create_notebook_page("Текст", Some(&c)).unwrap();
        assert!(b.update_group(&a, "A", Some(&c), false).is_err());
        // Скрытие пришло только в реестр, дочерние файлы ещё не обновлены.
        let mut groups = b.groups().unwrap();
        groups[0].hidden = true;
        b.write_file(GROUPS_FILE, serialize_groups(&groups).as_bytes()).unwrap();
        b.update_notebook_page(&p, |p| {
            p.group = None;
            Ok(())
        })
        .unwrap();
        assert!(b.notebook_page(&p).unwrap().hidden);
        b.update_group(&c, "Новое имя", None, false).unwrap();
        assert!(b.groups().unwrap().iter().find(|g| g.id == c).unwrap().hidden);
    }
    #[test]
    fn markdown_conflicts_and_deletions_preserve_both_versions() {
        let b = book();
        let id = b.create_notebook_page("Rust", None).unwrap();
        let mut a = b.notebook_page(&id).unwrap();
        let mut other = a.clone();
        a.body.push_str("\n\nВерсия компьютера");
        other.body.push_str("\n\nВерсия телефона");
        let merged = merge_pages(&a.serialize(), &other.serialize()).unwrap();
        let page = Page::parse(&merged).unwrap();
        assert!(page.conflicted());
        assert!(page.body.contains("Версия компьютера") && page.body.contains("Версия телефона"));
        assert_eq!(merge_pages(&merged, &other.serialize()).unwrap(), merged);
        assert_eq!(merge_pages(&other.serialize(), &a.serialize()).unwrap(), merged);
        other.deleted = true;
        assert!(Page::parse(&merge_pages(&merged, &other.serialize()).unwrap()).unwrap().deleted);
        b.update_notebook_page(&id, |p| {
            p.deleted = true;
            Ok(())
        })
        .unwrap();
        assert!(
            b.update_notebook_page(&id, |p| {
                p.body = "# Возрождение".into();
                Ok(())
            })
            .is_err()
        );
    }
    #[test]
    fn invalid_edits_do_not_overwrite_files_and_deleted_groups_move_contents() {
        let b = book();
        let group = b.create_group("Языки", None).unwrap();
        let child = b.create_group("Rust", Some(&group)).unwrap();
        let id = b.create_notebook_page("Справочник", Some(&group)).unwrap();
        let before = b.notebook_page(&id).unwrap();
        assert!(
            b.update_notebook_page(&id, |p| {
                p.body.clear();
                Ok(())
            })
            .is_err()
        );
        assert_eq!(b.notebook_page(&id).unwrap(), before);
        b.update_group(&group, "Языки", None, true).unwrap();
        assert_eq!(b.notebook_page(&id).unwrap().group, None);
        assert!(!b.notebook_page(&id).unwrap().hidden);
        assert!(b.groups().unwrap().iter().any(|g| g.id == child && g.parent.is_none()));
        assert!(group_hidden(&b.groups().unwrap(), Some(&group)));
        assert!(group_hidden(&b.groups().unwrap(), Some("missing")));
        assert!(Page::parse("<!-- ztaskmgr note 2 -->").is_err());
        assert!(b.notebook_page("../escape").is_err());
    }
    #[test]
    fn references_survive_storage_merge_and_carry() {
        let mut task = Task::new("Апробация");
        task.id = Some("task1".into());
        task.set_references(&["abcd".into(), "ef12".into()]);
        let mut board = crate::model::Board::default();
        board.push(crate::model::ListId::WORK, task.clone());
        let text = storage::serialize(&board, &Lists::default());
        let parsed = storage::parse(&text, &Lists::default()).unwrap();
        assert_eq!(parsed.tasks().next().unwrap().references(), ["abcd", "ef12"]);
        assert_eq!(task.carried().references(), ["abcd", "ef12"]);
        let b = book();
        let id = b.create_notebook_page("Rust", None).unwrap();
        assert!(b.files().unwrap().contains(&format!("pages/{id}.md")));
        assert!(
            merge::merge_text(
                &format!("pages/{id}.md"),
                None,
                &b.notebook_page(&id).unwrap().serialize(),
                &Lists::default()
            )
            .unwrap()
            .is_some()
        );
    }
}
