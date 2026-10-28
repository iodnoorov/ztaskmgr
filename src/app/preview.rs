//! Данные панели превью: история задачи по страницам и её записи.

use super::*;

/// Что показывает превью: история задачи по страницам и её записи. Собирается заново, когда
/// меняется задача или файлы её страницы и записей (`key`).
pub struct Preview {
    pub(super) key: (Option<String>, Option<Stamp>, Option<Stamp>),
    pub history: Vec<(Date, ListId, State)>,
    pub notes: Notes,
}

impl App {
    /// Подготовка к кадру: данные превью для выделенной задачи, собираются заново, только
    /// если что-то поменялось. Отрисовка их только читает (`preview`).
    pub fn prepare(&mut self) {
        let _ = self.refresh_preview();
    }

    /// Готовые данные превью — для выделенной задачи; `None`, если её нет.
    pub fn preview(&self) -> Option<&Preview> {
        let index = self.selected()?;
        (self.preview_target == Some((self.date, self.active, index))).then_some(())?;
        self.preview.as_ref()
    }

    fn refresh_preview(&mut self) -> Option<()> {
        let index = self.selected()?;
        let target = (self.date, self.active, index);
        if self.preview_target != Some(target) {
            self.preview_target = Some(target);
            self.preview_scroll = 0;
        }
        let id = self.selected_task()?.id.clone();
        let notes_stamp = id.as_deref().and_then(|id| self.book.notes_stamp(id));
        let key = (id, self.page_stamp, notes_stamp);
        if self.preview.as_ref().is_none_or(|p| p.key != key) {
            let (history, notes) = match &key.0 {
                Some(id) => (self.book.history(id).unwrap_or_default(), self.book.load_notes(id).unwrap_or_default()),
                None => (Vec::new(), Notes::default()),
            };
            self.preview = Some(Preview { key, history, notes });
        }
        Some(())
    }

    /// Размер экрана поменялся (и при запуске).
    pub fn on_resize(&mut self, width: u16) {
        self.width = width;
    }
}
