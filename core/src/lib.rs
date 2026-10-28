//! Ядро ztaskmgr: книжка (страницы, списки, записи), её формат, слияние правок и синхронизация
//! с WebDAV. Без интерфейса — на нём стоят и программа для терминала, и приложение для Android.

pub mod book;
pub mod clock;
pub mod config;
pub mod crypt;
pub mod lists;
pub mod merge;
pub mod model;
pub mod notes;
pub mod page;
#[cfg(test)]
mod proptests;
pub mod storage;
pub mod store;
pub mod webdav;
