//! Книжка: каталог со страницами по датам, `2026-10-26.md` — четыре списка на этот день.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::clock::Date;
use crate::model::{Board, Category, State, Task};
use crate::storage;

pub struct Book {
    dir: PathBuf,
}

impl Book {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn page_path(&self, date: Date) -> PathBuf {
        self.dir.join(format!("{date}.md"))
    }

    /// Страница на дату; несуществующая — пустая.
    pub fn load(&self, date: Date) -> Result<Board> {
        storage::load(&self.page_path(date))
    }

    /// Сохраняет страницу. Пустая страница — это отсутствие файла: если все задачи удалены,
    /// файл удаляется, а пустой новый не создаётся.
    pub fn save(&self, date: Date, board: &Board) -> Result<()> {
        let path = self.page_path(date);
        if !board.is_empty() {
            return storage::save(&path, board);
        }
        match fs::remove_file(&path) {
            Err(e) if e.kind() != ErrorKind::NotFound => {
                Err(e).with_context(|| format!("удаление {}", path.display()))
            }
            _ => Ok(()),
        }
    }

    /// Даты всех страниц книжки, по возрастанию. Посторонние файлы не учитываются.
    pub fn dates(&self) -> Result<Vec<Date>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("чтение {}", self.dir.display())),
        };
        let mut dates: Vec<Date> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name();
                Date::parse_iso(name.to_str()?.strip_suffix(".md")?)
            })
            .collect();
        dates.sort();
        Ok(dates)
    }

    /// Автоперенос в начале дня: невыполненные задачи с последней заполненной страницы до
    /// `today` переезжают в конец тех же списков на `today`, а там помечаются `[>]`.
    /// Возвращает дату, с которой перенесли, и сколько задач.
    pub fn carry_over(&self, today: Date) -> Result<Option<(Date, usize)>> {
        let Some(from) = self.dates()?.into_iter().rev().find(|&d| d < today) else {
            return Ok(None);
        };
        let mut source = self.load(from)?;
        let moved = source.migrate_open(today);
        if moved.is_empty() {
            return Ok(None);
        }
        let count = moved.len();
        let mut target = self.load(today)?;
        for (category, task) in moved {
            target.push(category, task);
        }
        // Сначала новая страница: при сбое между записями задача окажется в двух местах,
        // а не пропадёт.
        self.save(today, &target)?;
        self.save(from, &source)?;
        Ok(Some((from, count)))
    }

    /// Ручной перенос: копия задачи — в конец того же списка на странице `to`, на текущей
    /// странице (`board`, ещё не сохранена) задача помечается `[>]`.
    pub fn postpone(
        &self,
        board: &mut Board,
        from: Date,
        category: Category,
        index: usize,
        to: Date,
    ) -> Result<bool> {
        let Some(task) = board.get_mut(category, index).filter(|t| t.is_open() && from != to)
        else {
            return Ok(false);
        };
        let mut target = self.load(to)?;
        target.push(category, Task::new(task.text.clone()));
        self.save(to, &target)?;
        task.state = State::Migrated(to);
        self.save(from, board)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(day: u32) -> Date {
        Date::new(2026, 10, day).unwrap()
    }

    fn temp_book(name: &str) -> Book {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-book-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Book::new(dir)
    }

    fn texts(board: &Board, category: Category) -> Vec<(String, State)> {
        board
            .list(category)
            .iter()
            .map(|t| (t.text.clone(), t.state))
            .collect()
    }

    #[test]
    fn empty_page_has_no_file() {
        let book = temp_book("empty");
        let mut board = Board::default();
        book.save(d(26), &board).unwrap();
        assert!(!book.page_path(d(26)).exists());

        board.push(Category::Work, Task::new("a"));
        book.save(d(26), &board).unwrap();
        assert!(book.page_path(d(26)).exists());
        fs::write(book.dir().join("notes.txt"), "").unwrap();
        assert_eq!(book.dates().unwrap(), [d(26)]);

        board.remove(Category::Work, 0);
        book.save(d(26), &board).unwrap();
        assert!(!book.page_path(d(26)).exists());
        assert!(book.dates().unwrap().is_empty());
        fs::remove_dir_all(book.dir()).unwrap();
    }

    #[test]
    fn carry_over_from_last_page() {
        let book = temp_book("carry");
        assert_eq!(book.carry_over(d(26)).unwrap(), None);

        let mut older = Board::default();
        older.push(Category::Home, Task::new("старая"));
        book.save(d(20), &older).unwrap();

        let mut last = Board::default();
        last.push(Category::Work, Task::new("доделать"));
        last.push(Category::Work, Task::new("готово"));
        last.get_mut(Category::Work, 1).unwrap().state = State::Done;
        last.push(Category::Personal, Task::new("позвонить"));
        book.save(d(23), &last).unwrap();

        // Страница на сегодня уже заполнена заранее: перенесённое — в конец.
        let mut today = Board::default();
        today.push(Category::Work, Task::new("план"));
        book.save(d(26), &today).unwrap();

        assert_eq!(book.carry_over(d(26)).unwrap(), Some((d(23), 2)));
        let today = book.load(d(26)).unwrap();
        assert_eq!(
            texts(&today, Category::Work),
            [("план".into(), State::Open), ("доделать".into(), State::Open)]
        );
        assert_eq!(texts(&today, Category::Personal), [("позвонить".into(), State::Open)]);
        let last = book.load(d(23)).unwrap();
        assert_eq!(
            texts(&last, Category::Work),
            [
                ("доделать".into(), State::Migrated(d(26))),
                ("готово".into(), State::Done)
            ]
        );
        // Только с последней страницы; повторный запуск ничего не делает.
        assert_eq!(book.load(d(20)).unwrap(), older);
        assert_eq!(book.carry_over(d(26)).unwrap(), None);
        fs::remove_dir_all(book.dir()).unwrap();
    }

    #[test]
    fn postpone_to_date() {
        let book = temp_book("postpone");
        let mut board = Board::default();
        board.push(Category::Other, Task::new("в пятницу"));
        board.push(Category::Other, Task::new("сделано"));
        board.get_mut(Category::Other, 1).unwrap().state = State::Done;

        assert!(book.postpone(&mut board, d(26), Category::Other, 0, d(30)).unwrap());
        assert_eq!(board.list(Category::Other)[0].state, State::Migrated(d(30)));
        assert_eq!(book.load(d(26)).unwrap(), board);
        assert_eq!(
            texts(&book.load(d(30)).unwrap(), Category::Other),
            [("в пятницу".into(), State::Open)]
        );
        // Выполненную, уже перенесённую и на ту же дату — нельзя.
        assert!(!book.postpone(&mut board, d(26), Category::Other, 1, d(30)).unwrap());
        assert!(!book.postpone(&mut board, d(26), Category::Other, 0, d(31)).unwrap());
        let mut fresh = Board::default();
        fresh.push(Category::Work, Task::new("x"));
        assert!(!book.postpone(&mut fresh, d(26), Category::Work, 0, d(26)).unwrap());
        fs::remove_dir_all(book.dir()).unwrap();
    }
}
