//! Окно списков: добавить, переименовать, удалить (в архив), порядок, скрыть от ИИ.

use super::*;

/// Что делается в окне списков.
#[derive(Debug)]
pub enum ListsMode {
    Browse,
    /// Название нового списка.
    Add(LineInput),
    Rename(LineInput),
    /// Удаление выделенного списка. С задачами на сегодня и дальше (`count`) — выбор, куда
    /// их перенести (`target` — номер в `delete_targets`); без них — просто подтверждение.
    Delete {
        count: usize,
        target: usize,
    },
}

impl App {
    /// Строки окна списков: те, что на экране, по порядку, затем архивные.
    pub fn list_rows(&self) -> Vec<ListId> {
        let mut rows = self.registry.active();
        rows.extend(self.registry.all().iter().filter(|d| d.archived).map(|d| d.id));
        rows
    }

    /// Куда перенести задачи удаляемого списка.
    pub fn delete_targets(&self, list: ListId) -> Vec<ListId> {
        self.registry.active().into_iter().filter(|&c| c != list).collect()
    }

    /// Окно списков. Состояние вынимается из `mode` на время обработки и кладётся обратно,
    /// если окно не закрыли.
    pub(super) fn on_key_lists(&mut self, press: Press) {
        let Mode::Lists { row, sub } = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return;
        };
        let rows = self.list_rows();
        let mut row = row.min(rows.len() - 1);
        let list = rows[row];
        let archived = !self.registry.is_active(list);
        let code = press.hotkey.code;
        let sub = match sub {
            ListsMode::Browse => match (code, press.hotkey.modifiers.contains(KeyModifiers::SHIFT)) {
                (KeyCode::Up | KeyCode::Down, true) => {
                    let up = code == KeyCode::Up;
                    let mut moved = false;
                    self.edit_registry(|lists| {
                        moved = lists.shift(list, up);
                        Ok(())
                    });
                    if moved {
                        row = if up { row - 1 } else { row + 1 };
                    }
                    ListsMode::Browse
                }
                (KeyCode::Char('j') | KeyCode::Down, _) => {
                    row = (row + 1).min(rows.len() - 1);
                    ListsMode::Browse
                }
                (KeyCode::Char('k') | KeyCode::Up, _) => {
                    row = row.saturating_sub(1);
                    ListsMode::Browse
                }
                (KeyCode::Char('a'), _) => ListsMode::Add(LineInput::default()),
                (KeyCode::Char('e') | KeyCode::Enter, _) if !archived => {
                    ListsMode::Rename(LineInput::new(self.list_title(list).to_string()))
                }
                (KeyCode::Char('r'), _) if archived => {
                    if self.edit_registry(|lists| lists.restore(list)) {
                        row = self.registry.active().len() - 1;
                        self.status = Some(Status::Info(format!("Список «{}» снова на экране", self.list_title(list))));
                    }
                    ListsMode::Browse
                }
                (KeyCode::Char('d') | KeyCode::Delete, _) if !archived => self.start_delete(list),
                (KeyCode::Char('p' | 'P'), _) => {
                    self.toggle_hidden_of(list);
                    ListsMode::Browse
                }
                (KeyCode::Esc | KeyCode::Char('q' | 'L'), _) => return,
                _ => ListsMode::Browse,
            },
            ListsMode::Add(mut input) => match edit_line(&mut input, &press) {
                Some(KeyCode::Enter) => {
                    let id = match self.book.new_id() {
                        Ok(id) => ListId::new(&id),
                        Err(e) => {
                            self.status = Some(Status::Error(format!("Новый список: {e:#}")));
                            self.mode = Mode::Lists { row, sub: ListsMode::Add(input) };
                            return;
                        }
                    };
                    let title = input.text().to_string();
                    if self.edit_registry(|lists| lists.add(id, &title)) {
                        row = self.registry.active().len() - 1;
                        self.status = Some(Status::Info(format!("Список «{}» добавлен", self.list_title(id))));
                        ListsMode::Browse
                    } else {
                        ListsMode::Add(input)
                    }
                }
                Some(_) => ListsMode::Browse,
                None => ListsMode::Add(input),
            },
            ListsMode::Rename(mut input) => match edit_line(&mut input, &press) {
                Some(KeyCode::Enter) => {
                    let title = input.text().to_string();
                    if self.edit_registry(|lists| lists.rename(list, &title)) {
                        ListsMode::Browse
                    } else {
                        ListsMode::Rename(input)
                    }
                }
                Some(_) => ListsMode::Browse,
                None => ListsMode::Rename(input),
            },
            ListsMode::Delete { count: 0, .. } => {
                if matches!(code, KeyCode::Char('y') | KeyCode::Enter) {
                    self.finish_delete(list, None);
                    row = row.min(self.list_rows().len() - 1);
                }
                ListsMode::Browse
            }
            ListsMode::Delete { count, mut target } => {
                let targets = self.delete_targets(list);
                let to = match code {
                    KeyCode::Char(c @ '1'..='9') => targets.get(c as usize - '1' as usize).copied(),
                    KeyCode::Enter => targets.get(target).copied(),
                    KeyCode::Char('j') | KeyCode::Down => {
                        target = (target + 1).min(targets.len() - 1);
                        None
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        target = target.saturating_sub(1);
                        None
                    }
                    _ => {
                        self.mode = Mode::Lists { row, sub: ListsMode::Browse };
                        return;
                    }
                };
                match to {
                    Some(to) => {
                        self.finish_delete(list, Some(to));
                        row = row.min(self.list_rows().len() - 1);
                        ListsMode::Browse
                    }
                    None => ListsMode::Delete { count, target },
                }
            }
        };
        self.mode = Mode::Lists { row, sub };
    }

    /// `d` в окне списков: последний не удаляется; есть задачи на сегодня и дальше — сначала
    /// выбрать, куда их перенести.
    pub(super) fn start_delete(&mut self, list: ListId) -> ListsMode {
        if self.registry.active().len() == 1 {
            self.status = Some(Status::Error("Последний список удалить нельзя".into()));
            return ListsMode::Browse;
        }
        match self.book.tasks_since(list, self.today) {
            Ok(count) => ListsMode::Delete { count, target: 0 },
            Err(e) => {
                self.status = Some(Status::Error(format!("Удаление списка: {e:#}")));
                ListsMode::Browse
            }
        }
    }

    /// Удаление списка: задачи с сегодняшней страницы и дальше — в `to`; если задачи в нём
    /// когда-нибудь были, он уходит в архив (прошлые страницы показывают его как было),
    /// иначе удаляется совсем.
    pub(super) fn finish_delete(&mut self, list: ListId, to: Option<ListId>) {
        let title = self.list_title(list).to_string();
        let (registry, moved, used) = match self.book.delete_list(list, to, self.today) {
            Ok(result) => result,
            Err(e) => {
                self.status = Some(Status::Error(format!("Удаление списка: {e:#}")));
                return;
            }
        };
        self.registry = registry;
        self.lists_stamp = self.book.lists_stamp();
        self.edited = true;
        self.reload_page();
        self.status = Some(Status::Info(match to {
            Some(to) if moved > 0 => format!("Список «{title}» удалён, задачи ({moved}) — в «{}»", self.list_title(to)),
            _ if used => format!("Список «{title}» удалён; на прошлых страницах он остался"),
            _ => format!("Список «{title}» удалён"),
        }));
    }
}
