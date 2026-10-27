//! Реестр списков книжки — `LISTS.md`: какие списки есть, в каком порядке они на экране,
//! как называются, скрыты ли от ИИ, убраны ли в архив. Синхронизируется вместе с книжкой.
//!
//! ```markdown
//! - Работа <!-- list:work -->
//! - Личное <!-- list:personal hidden -->
//! - Ремонт <!-- list:k3x9ab archived -->
//! ```
//!
//! Нет файла — четыре списка новой книжки. Страницы ссылаются на списки по id
//! (`## Работа <!-- list:work -->`), поэтому переименование не трогает страниц, а удалённый
//! список, в котором были задачи, уходит в архив: на прошлых страницах он виден как был.

use anyhow::{Context, Result, bail};

use crate::model::ListId;
use crate::storage::is_valid_id;

pub const LISTS_FILE: &str = "LISTS.md";

/// Больше списков в сетку 5×5 не помещается.
pub const MAX_LISTS: usize = 25;

const HEADER: &str = "# Списки: порядок — как на экране. Название можно менять, id после него — нет.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListDef {
    pub id: ListId,
    pub title: String,
    /// Скрыт от ИИ.
    pub hidden: bool,
    /// Удалён, но в нём были задачи: на экране — только на страницах, где они есть.
    pub archived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lists {
    defs: Vec<ListDef>,
}

impl Default for Lists {
    fn default() -> Self {
        let titles = ["Работа", "Дом", "Личное", "Прочее"];
        Self {
            defs: ListId::DEFAULTS
                .into_iter()
                .zip(titles)
                .map(|(id, title)| ListDef { id, title: title.into(), hidden: false, archived: false })
                .collect(),
        }
    }
}

/// Служебное в конце строки: `<!-- list:work hidden archived -->`.
fn parse_def(line: &str) -> Option<ListDef> {
    let rest = line.strip_prefix("- ")?.strip_suffix(" -->")?;
    let (title, comment) = rest.rsplit_once(" <!-- ")?;
    let mut def = ListDef { id: ListId::WORK, title: title.trim().to_string(), hidden: false, archived: false };
    let mut id = None;
    for word in comment.split_whitespace() {
        match word {
            "hidden" => def.hidden = true,
            "archived" => def.archived = true,
            _ => id = Some(word.strip_prefix("list:").filter(|id| is_valid_id(id))?),
        }
    }
    def.id = ListId::new(id?);
    (!def.title.is_empty()).then_some(def)
}

impl Lists {
    pub fn parse(text: &str) -> Result<Self> {
        let mut defs: Vec<ListDef> = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let def = parse_def(line)
                .with_context(|| format!("строка {}: ожидался список «- Название <!-- list:id -->»: {line}", n + 1))?;
            if defs.iter().any(|d| d.id == def.id) {
                bail!("строка {}: список {} указан дважды", n + 1, def.id.as_str());
            }
            defs.push(def);
        }
        if !defs.iter().any(|d| !d.archived) {
            bail!("нет ни одного списка");
        }
        Ok(Self { defs })
    }

    pub fn serialize(&self) -> String {
        let mut out = format!("{HEADER}\n\n");
        for def in &self.defs {
            out.push_str(&format!("- {} <!-- list:{}", def.title, def.id.as_str()));
            if def.hidden {
                out.push_str(" hidden");
            }
            if def.archived {
                out.push_str(" archived");
            }
            out.push_str(" -->\n");
        }
        out
    }

    /// Все списки, с архивными, в порядке файла.
    pub fn all(&self) -> &[ListDef] {
        &self.defs
    }

    /// Списки на экране, по порядку.
    pub fn active(&self) -> Vec<ListId> {
        self.defs.iter().filter(|d| !d.archived).map(|d| d.id).collect()
    }

    /// Списки страницы по порядку: те, что на экране, затем остальные из `extra` (архивные
    /// и незнакомые списки, в которых на странице есть задачи).
    pub fn order(&self, extra: impl IntoIterator<Item = ListId>) -> Vec<ListId> {
        let mut order = self.active();
        for id in extra {
            if !order.contains(&id) {
                order.push(id);
            }
        }
        order
    }

    /// Название для показа; незнакомый список — его id.
    pub fn label(&self, id: ListId) -> &str {
        self.title(id).unwrap_or(id.as_str())
    }

    pub fn get(&self, id: ListId) -> Option<&ListDef> {
        self.defs.iter().find(|d| d.id == id)
    }

    fn get_mut(&mut self, id: ListId) -> Result<&mut ListDef> {
        self.defs.iter_mut().find(|d| d.id == id).context("нет такого списка")
    }

    pub fn title(&self, id: ListId) -> Option<&str> {
        self.get(id).map(|d| d.title.as_str())
    }

    /// Список по названию (без учёта регистра; сначала среди тех, что на экране) или по id.
    pub fn find(&self, name: &str) -> Option<ListId> {
        let name = name.trim().to_lowercase();
        let by_title = |archived: bool| {
            self.defs
                .iter()
                .find(|d| d.archived == archived && d.title.to_lowercase() == name)
        };
        by_title(false)
            .or_else(|| by_title(true))
            .or_else(|| self.defs.iter().find(|d| d.id.as_str() == name))
            .map(|d| d.id)
    }

    /// Список по заголовку страницы без id: по названию, а для страниц старых версий —
    /// по названиям четырёх исходных списков, даже если их переименовали.
    pub fn resolve_heading(&self, title: &str) -> Option<ListId> {
        let title = title.trim().to_lowercase();
        let by_title = |defs: &[ListDef]| {
            let mut found = defs.iter().filter(|d| d.title.to_lowercase() == title);
            // Сначала среди тех, что на экране.
            found.clone().find(|d| !d.archived).or_else(|| found.next()).map(|d| d.id)
        };
        by_title(&self.defs).or_else(|| by_title(&Lists::default().defs))
    }

    pub fn is_hidden(&self, id: ListId) -> bool {
        self.get(id).is_some_and(|d| d.hidden)
    }

    pub fn is_active(&self, id: ListId) -> bool {
        self.get(id).is_some_and(|d| !d.archived)
    }

    /// Название без лишних пробелов, не пустое и не занятое другим списком на экране.
    /// `/` нельзя: ссылки на задачи агента — `дата/список/номер`.
    fn check_title(&self, title: &str, except: Option<ListId>) -> Result<String> {
        let title = title.trim();
        if title.is_empty() {
            bail!("пустое название");
        }
        if title.contains('/') || title.contains("<!--") || title.contains("-->") {
            bail!("в названии не может быть «/», «<!--» и «-->»");
        }
        let lower = title.to_lowercase();
        if self.defs.iter().any(|d| !d.archived && Some(d.id) != except && d.title.to_lowercase() == lower) {
            bail!("список «{title}» уже есть");
        }
        Ok(title.to_string())
    }

    fn check_room(&self) -> Result<()> {
        if self.active().len() >= MAX_LISTS {
            bail!("больше {MAX_LISTS} списков нельзя");
        }
        Ok(())
    }

    /// Новый список в конец.
    pub fn add(&mut self, id: ListId, title: &str) -> Result<()> {
        self.check_room()?;
        let title = self.check_title(title, None)?;
        if self.get(id).is_some() {
            bail!("список {} уже есть", id.as_str());
        }
        self.defs.push(ListDef { id, title, hidden: false, archived: false });
        Ok(())
    }

    pub fn rename(&mut self, id: ListId, title: &str) -> Result<()> {
        let title = self.check_title(title, Some(id))?;
        self.get_mut(id)?.title = title;
        Ok(())
    }

    pub fn set_hidden(&mut self, id: ListId, hidden: bool) -> Result<()> {
        self.get_mut(id)?.hidden = hidden;
        Ok(())
    }

    fn check_not_last(&self, id: ListId) -> Result<()> {
        if self.active() == [id] {
            bail!("последний список удалить нельзя");
        }
        Ok(())
    }

    /// В архив: со страниц, где у него нет задач, список пропадает.
    pub fn archive(&mut self, id: ListId) -> Result<()> {
        self.check_not_last(id)?;
        self.get_mut(id)?.archived = true;
        Ok(())
    }

    /// Совсем: в списке никогда не было задач.
    pub fn remove(&mut self, id: ListId) -> Result<()> {
        self.check_not_last(id)?;
        self.defs.retain(|d| d.id != id);
        Ok(())
    }

    /// Из архива — снова на экран, в конец. Название могли занять — тогда ошибка.
    pub fn restore(&mut self, id: ListId) -> Result<()> {
        self.check_room()?;
        let title = self.get(id).context("нет такого списка")?.title.clone();
        self.check_title(&title, Some(id))?;
        let n = self.defs.iter().position(|d| d.id == id).unwrap();
        let mut def = self.defs.remove(n);
        def.archived = false;
        self.defs.push(def);
        Ok(())
    }

    /// Меняет список местами с соседним на экране. Возвращает, сдвинулся ли.
    pub fn shift(&mut self, id: ListId, up: bool) -> bool {
        let active: Vec<usize> = (0..self.defs.len()).filter(|&i| !self.defs[i].archived).collect();
        let Some(pos) = active.iter().position(|&i| self.defs[i].id == id) else {
            return false;
        };
        let other = if up { pos.checked_sub(1) } else { Some(pos + 1).filter(|&p| p < active.len()) };
        match other {
            Some(other) => {
                self.defs.swap(active[pos], active[other]);
                true
            }
            None => false,
        }
    }

    /// Слияние с копией реестра с другого устройства: списков — объединение (новые встают
    /// после того, за которым стояли там), название и порядок — свои; скрыт или в архиве
    /// хоть где-то — так и остаётся.
    pub fn merge(&mut self, other: &Lists) {
        let mut after: Option<ListId> = None;
        for theirs in &other.defs {
            match self.defs.iter_mut().find(|d| d.id == theirs.id) {
                Some(mine) => {
                    mine.hidden |= theirs.hidden;
                    mine.archived |= theirs.archived;
                }
                None => {
                    let at = after
                        .and_then(|a| self.defs.iter().position(|d| d.id == a))
                        .map_or(self.defs.len(), |i| i + 1);
                    self.defs.insert(at, theirs.clone());
                }
            }
            after = Some(theirs.id);
        }
        // Всё ушло в архив на разных устройствах — один список остаётся.
        if self.active().is_empty()
            && let Some(first) = self.defs.first_mut()
        {
            first.archived = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_edits() {
        let mut lists = Lists::default();
        let text = lists.serialize();
        assert!(text.contains("- Работа <!-- list:work -->\n"), "{text}");
        assert_eq!(Lists::parse(&text).unwrap(), lists);

        let new = ListId::new("q1w2e3");
        lists.add(new, "  Ремонт ").unwrap();
        assert!(lists.add(ListId::new("q1w2e4"), "ремонт").is_err());
        assert!(lists.add(ListId::new("q1w2e5"), "a/b").is_err());
        assert!(lists.rename(ListId::HOME, "работа").is_err());
        lists.rename(ListId::HOME, "Квартира").unwrap();
        lists.set_hidden(ListId::PERSONAL, true).unwrap();
        assert!(lists.shift(new, true));
        assert!(!lists.shift(ListId::WORK, true));
        lists.archive(ListId::OTHER).unwrap();
        assert_eq!(lists.active(), [ListId::WORK, ListId::HOME, ListId::PERSONAL, new]);
        assert_eq!(lists.find("квартира"), Some(ListId::HOME));
        assert_eq!(lists.find("other"), Some(ListId::OTHER));
        let text = lists.serialize();
        assert!(text.contains("- Личное <!-- list:personal hidden -->\n"), "{text}");
        assert!(text.contains("- Прочее <!-- list:other archived -->\n"), "{text}");
        assert_eq!(Lists::parse(&text).unwrap(), lists);

        // Архивный — обратно в конец; занятое название не даёт вернуть.
        lists.restore(ListId::OTHER).unwrap();
        assert_eq!(lists.active().last(), Some(&ListId::OTHER));
        lists.archive(ListId::OTHER).unwrap();
        lists.add(ListId::new("q1w2e6"), "Прочее").unwrap();
        assert!(lists.restore(ListId::OTHER).is_err());

        // Последний не удаляется.
        let mut one = Lists::parse("- Один <!-- list:work -->\n").unwrap();
        assert!(one.archive(ListId::WORK).is_err() && one.remove(ListId::WORK).is_err());
        assert!(Lists::parse("- Один\n").is_err());
        assert!(Lists::parse("- Один <!-- list:work archived -->\n").is_err());
    }

    #[test]
    fn merge() {
        let mut pc = Lists::default();
        pc.add(ListId::new("aa1"), "С компьютера").unwrap();
        let mut phone = Lists::default();
        phone.rename(ListId::WORK, "Офис").unwrap();
        phone.archive(ListId::OTHER).unwrap();
        phone.add(ListId::new("bb2"), "С телефона").unwrap();
        phone.set_hidden(ListId::HOME, true).unwrap();
        pc.merge(&phone);
        let titles: Vec<&str> = pc.all().iter().map(|d| d.title.as_str()).collect();
        assert_eq!(titles, ["Работа", "Дом", "Личное", "Прочее", "С телефона", "С компьютера"]);
        assert!(pc.is_hidden(ListId::HOME) && !pc.is_active(ListId::OTHER));
    }
}
