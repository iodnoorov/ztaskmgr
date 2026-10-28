//! Открытая страница: доска дня и отпечаток файла, с которого она прочитана. Сохранение
//! не затирает то, что за это время записали снаружи (синхронизация, агент, другое окно),
//! а сливается с ним.

use anyhow::Result;

use crate::book::{Book, Stamp};
use crate::clock::Date;
use crate::merge::merge_boards;
use crate::model::Board;

pub struct Page {
    pub date: Date,
    pub board: Board,
    stamp: Option<Stamp>,
}

impl Page {
    pub fn open(book: &Book, date: Date) -> Result<Self> {
        let stamp = book.page_stamp(date);
        Ok(Self { date, board: book.load(date)?, stamp })
    }

    /// Пустая страница, не связанная с файлом: на месте той, что не прочиталась.
    pub fn empty(date: Date) -> Self {
        Self { date, board: Board::default(), stamp: None }
    }

    /// Файл поменяли снаружи с тех пор, как его читали или писали отсюда.
    pub fn changed_outside(&self, book: &Book) -> bool {
        book.page_stamp(self.date) != self.stamp
    }

    /// Перечитать с диска.
    pub fn reload(&mut self, book: &Book) -> Result<()> {
        *self = Self::open(book, self.date)?;
        Ok(())
    }

    /// Сохранить; правка снаружи, которой мы ещё не видели, сливается с нашей. Задачи без id
    /// получают id.
    pub fn save(&mut self, book: &Book) -> Result<()> {
        if self.changed_outside(book)
            && let Ok(disk) = book.load(self.date)
        {
            self.board = merge_boards(&self.board, &disk);
        }
        self.board.assign_ids(|| book.new_id());
        let result = book.save(self.date, &self.board);
        self.stamp = book.page_stamp(self.date);
        result
    }

    /// После того как `Book` сам записал страницу (`postpone`): запомнить новый отпечаток.
    pub fn saved_by_book(&mut self, book: &Book) {
        self.stamp = book.page_stamp(self.date);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ListId, Task};

    #[test]
    fn save_merges_outside_edit() {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-page-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let book = Book::new(dir.clone());
        let d = Date::new(2026, 10, 26).unwrap();
        let mut page = Page::open(&book, d).unwrap();
        page.board.push(ListId::WORK, Task::new("здесь"));
        page.save(&book).unwrap();
        assert!(!page.changed_outside(&book));

        // Снаружи дописали задачу — наша правка её не затирает.
        let mut outside = book.load(d).unwrap();
        outside.push(ListId::WORK, Task::new("снаружи"));
        book.save(d, &outside).unwrap();
        assert!(page.changed_outside(&book));
        page.board.push(ListId::WORK, Task::new("ещё здесь"));
        page.save(&book).unwrap();
        let texts: Vec<_> = book.load(d).unwrap().list(ListId::WORK).iter().map(|t| t.text.clone()).collect();
        assert_eq!(texts.len(), 3, "{texts:?}");
        assert!(page.board.tasks().all(|t| t.id.is_some()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
