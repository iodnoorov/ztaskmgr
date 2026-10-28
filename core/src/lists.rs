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

/// Версия формата книжки. 2 — списки с id; 3 — время правки и надгробия у задач и списков.
/// Книжку более нового формата эта версия не перезаписывает.
pub const FORMAT: u32 = 3;
const FORMAT_LINE: &str = "<!-- ztaskmgr format ";
/// Надгробия удалённых списков: `<!-- removed: k3x9ab@1730112345678 -->`.
const GRAVES: &str = "<!-- removed: ";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListDef {
    pub id: ListId,
    pub title: String,
    /// Скрыт от ИИ.
    pub hidden: bool,
    /// Удалён, но в нём были задачи: на экране — только на страницах, где они есть.
    pub archived: bool,
    /// Когда меняли название или пометки (мс; 0 — неизвестно). При слиянии — позднейшее.
    pub modified: u64,
    /// Незнакомые пометки `ключ:значение` от более новой версии — сохраняются как есть.
    pub extra: Vec<String>,
}

impl ListDef {
    fn new(id: ListId, title: String) -> Self {
        Self { id, title, hidden: false, archived: false, modified: 0, extra: Vec::new() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lists {
    defs: Vec<ListDef>,
    /// Надгробия удалённых списков: при слиянии они не возвращаются.
    removed: Vec<(ListId, u64)>,
}

impl Default for Lists {
    fn default() -> Self {
        let titles = ["Работа", "Дом", "Личное", "Прочее"];
        Self {
            defs: ListId::DEFAULTS.into_iter().zip(titles).map(|(id, title)| ListDef::new(id, title.into())).collect(),
            removed: Vec::new(),
        }
    }
}

/// Служебное в конце строки: `<!-- list:work t:1730112345678 hidden archived -->`.
fn parse_def(line: &str) -> Option<ListDef> {
    let rest = line.strip_prefix("- ")?.strip_suffix(" -->")?;
    let (title, comment) = rest.rsplit_once(" <!-- ")?;
    let mut def = ListDef::new(ListId::WORK, title.trim().to_string());
    let mut id = None;
    for word in comment.split_whitespace() {
        match word.split_once(':').unwrap_or((word, "")) {
            ("hidden", "") => def.hidden = true,
            ("archived", "") => def.archived = true,
            ("list", value) => id = Some(value).filter(|id| is_valid_id(id)),
            ("t", value) => def.modified = value.parse().ok()?,
            (key, value) if !key.is_empty() && !value.is_empty() => def.extra.push(word.to_string()),
            _ => return None,
        }
    }
    def.id = ListId::new(id?);
    (!def.title.is_empty()).then_some(def)
}

fn now() -> u64 {
    crate::clock::now_ms()
}

impl Lists {
    pub fn parse(text: &str) -> Result<Self> {
        let mut defs: Vec<ListDef> = Vec::new();
        let mut removed = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(version) = line.strip_prefix(FORMAT_LINE).and_then(|l| l.strip_suffix(" -->")) {
                let version: u32 =
                    version.trim().parse().with_context(|| format!("строка {}: версия формата", n + 1))?;
                if version > FORMAT {
                    bail!(
                        "книжка записана более новой версией ztaskmgr (формат {version}, эта понимает до {FORMAT}) — обновите программу"
                    );
                }
                continue;
            }
            if let Some(graves) = line.strip_prefix(GRAVES).and_then(|l| l.strip_suffix(" -->")) {
                for grave in graves.split_whitespace() {
                    let (id, when) = grave
                        .split_once('@')
                        .and_then(|(id, t)| Some((id, t.parse::<u64>().ok()?)))
                        .filter(|(id, _)| is_valid_id(id))
                        .with_context(|| format!("строка {}: испорченное надгробие «{grave}»", n + 1))?;
                    removed.push((ListId::new(id), when));
                }
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
        Ok(Self { defs, removed })
    }

    pub fn serialize(&self) -> String {
        let mut out = format!("{HEADER}\n{FORMAT_LINE}{FORMAT} -->\n\n");
        for def in &self.defs {
            out.push_str(&format!("- {} <!-- list:{}", def.title, def.id.as_str()));
            if def.modified > 0 {
                out.push_str(&format!(" t:{}", def.modified));
            }
            if def.hidden {
                out.push_str(" hidden");
            }
            if def.archived {
                out.push_str(" archived");
            }
            for word in &def.extra {
                out.push(' ');
                out.push_str(word);
            }
            out.push_str(" -->\n");
        }
        if !self.removed.is_empty() {
            let graves: Vec<String> = self.removed.iter().map(|(id, t)| format!("{}@{t}", id.as_str())).collect();
            out.push_str(&format!("\n{GRAVES}{} -->\n", graves.join(" ")));
        }
        out
    }

    fn bury(&mut self, id: ListId, when: u64) {
        match self.removed.iter_mut().find(|(i, _)| *i == id) {
            Some((_, t)) => *t = (*t).max(when),
            None => self.removed.push((id, when)),
        }
    }

    fn buried(&self, id: ListId) -> Option<u64> {
        self.removed.iter().find(|(i, _)| *i == id).map(|(_, t)| *t)
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
        let by_title =
            |archived: bool| self.defs.iter().find(|d| d.archived == archived && d.title.to_lowercase() == name);
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
        let mut def = ListDef::new(id, title);
        def.modified = now();
        self.defs.push(def);
        Ok(())
    }

    pub fn rename(&mut self, id: ListId, title: &str) -> Result<()> {
        let title = self.check_title(title, Some(id))?;
        let def = self.get_mut(id)?;
        def.title = title;
        def.modified = now();
        Ok(())
    }

    pub fn set_hidden(&mut self, id: ListId, hidden: bool) -> Result<()> {
        let def = self.get_mut(id)?;
        def.hidden = hidden;
        def.modified = now();
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
        let def = self.get_mut(id)?;
        def.archived = true;
        def.modified = now();
        Ok(())
    }

    /// Совсем: в списке никогда не было задач. Остаётся надгробие — чтобы при слиянии
    /// с другим устройством список не вернулся.
    pub fn remove(&mut self, id: ListId) -> Result<()> {
        self.check_not_last(id)?;
        self.defs.retain(|d| d.id != id);
        self.bury(id, now());
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
        def.modified = now();
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
    /// после того, за которым стояли там), порядок — свой. У одного и того же списка —
    /// позднейшая правка (название, «скрыт», «в архиве»); время неизвестно или одинаково —
    /// скрыт или в архиве хоть где-то. Удалённый список (надгробие) не возвращается, если его
    /// не меняли после удаления.
    pub fn merge(&mut self, other: &Lists) {
        for &(id, when) in &other.removed {
            self.bury(id, when);
        }
        let mut after: Option<ListId> = None;
        for theirs in &other.defs {
            let buried = self.buried(theirs.id);
            match self.defs.iter_mut().find(|d| d.id == theirs.id) {
                Some(mine) if theirs.modified > mine.modified => *mine = theirs.clone(),
                Some(mine) if theirs.modified == mine.modified => {
                    mine.hidden |= theirs.hidden;
                    mine.archived |= theirs.archived;
                }
                Some(_) => {}
                None if buried.is_some_and(|when| when >= theirs.modified) => {}
                None => {
                    let at =
                        after.and_then(|a| self.defs.iter().position(|d| d.id == a)).map_or(self.defs.len(), |i| i + 1);
                    self.defs.insert(at, theirs.clone());
                }
            }
            after = Some(theirs.id);
        }
        let removed = &self.removed;
        self.defs.retain(|d| {
            let when = removed.iter().find(|(i, _)| *i == d.id).map(|(_, t)| *t);
            !when.is_some_and(|when| when >= d.modified)
        });
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
        let line = |prefix: &str, suffix: &str| text.lines().any(|l| l.starts_with(prefix) && l.ends_with(suffix));
        assert!(line("- Личное <!-- list:personal t:", " hidden -->"), "{text}");
        assert!(line("- Прочее <!-- list:other t:", " archived -->"), "{text}");
        assert!(text.contains("<!-- ztaskmgr format 3 -->"), "{text}");
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
        crate::clock::freeze(Some(1_000));
        let mut pc = Lists::default();
        pc.add(ListId::new("aa1"), "С компьютера").unwrap();
        let mut phone = Lists::default();
        phone.rename(ListId::WORK, "Офис").unwrap();
        phone.archive(ListId::OTHER).unwrap();
        phone.add(ListId::new("bb2"), "С телефона").unwrap();
        phone.set_hidden(ListId::HOME, true).unwrap();
        pc.merge(&phone);
        // Позднейшие правки с телефона — и название, и пометки.
        let titles: Vec<&str> = pc.all().iter().map(|d| d.title.as_str()).collect();
        assert_eq!(titles, ["Офис", "Дом", "Личное", "Прочее", "С телефона", "С компьютера"]);
        assert!(pc.is_hidden(ListId::HOME) && !pc.is_active(ListId::OTHER));

        // Открыть для ИИ позже, чем скрыли на другом устройстве, — открыт (раньше «скрыт»
        // побеждал всегда).
        let mut later = pc.clone();
        crate::clock::freeze(Some(2_000));
        later.set_hidden(ListId::HOME, false).unwrap();
        let mut merged = pc.clone();
        merged.merge(&later);
        assert!(!merged.is_hidden(ListId::HOME));

        // Удалённый список не возвращается, если его не меняли после удаления.
        let mut removed = pc.clone();
        crate::clock::freeze(Some(3_000));
        removed.remove(ListId::new("aa1")).unwrap();
        let mut merged = pc.clone();
        merged.merge(&removed);
        assert!(merged.get(ListId::new("aa1")).is_none());
        let mut back = removed.clone();
        back.merge(&pc);
        assert!(back.get(ListId::new("aa1")).is_none());
        assert_eq!(Lists::parse(&removed.serialize()).unwrap(), removed);
        crate::clock::freeze(None);
    }

    #[test]
    fn newer_format_is_refused() {
        let text = "<!-- ztaskmgr format 99 -->\n- Работа <!-- list:work future:yes -->\n";
        let err = Lists::parse(text).unwrap_err();
        assert!(format!("{err:#}").contains("обновите программу"), "{err:#}");
        // Незнакомые пометки текущего формата — сохраняются.
        let lists = Lists::parse("- Работа <!-- list:work future:yes -->\n").unwrap();
        assert!(lists.serialize().contains("- Работа <!-- list:work future:yes -->"));
    }
}
