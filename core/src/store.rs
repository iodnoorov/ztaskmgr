//! Где лежат файлы книжки. `Book` работает с ними только через `Store` — по относительному
//! пути (`2026-10-26.md`, `notes/k3x9ab.md`), не зная, файловая это система или что-то другое.
//! Сейчас это каталог на диске (`FsStore`); в тестах — память (`MemStore`). Так же сюда
//! встанет, например, шифрование книжки — обёрткой над другим `Store`.

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

/// Отпечаток файла: время изменения, размер и inode. По нему видно правки снаружи (агент,
/// синхронизация, редактор). Время в Linux грубое (миллисекунды), две записи подряд могут
/// его не сдвинуть, поэтому ещё размер и inode: запись через rename всегда даёт новый inode.
pub type Stamp = (SystemTime, u64, u64);

pub trait Store {
    /// Содержимое файла; нет файла — `None`.
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>>;
    /// Запись целиком и атомарно: сбой посреди записи не портит файл.
    fn write(&self, path: &str, data: &[u8]) -> Result<()>;
    /// Удаление; файла и так нет — не ошибка.
    fn remove(&self, path: &str) -> Result<()>;
    /// Имена файлов в каталоге (`""` — корень книжки, `"notes"`); нет каталога — пусто.
    fn list(&self, dir: &str) -> Result<Vec<String>>;
    /// Отпечаток; нет файла — `None`.
    fn stamp(&self, path: &str) -> Option<Stamp>;
}

/// Книжка — каталог на диске.
pub struct FsStore {
    dir: PathBuf,
}

impl FsStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, path: &str) -> PathBuf {
        self.dir.join(path)
    }
}

/// Пишет через временный файл и rename, чтобы сбой посреди записи не испортил данные.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).with_context(|| format!("создание {}", dir.display()))?;
    }
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = Path::new(&tmp_name);
    let mut file = fs::File::create(tmp).with_context(|| format!("запись {}", tmp.display()))?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, path).with_context(|| format!("переименование в {}", path.display()))
}

impl Store for FsStore {
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let full = self.path(path);
        match fs::read(&full) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("чтение {}", full.display())),
        }
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        write_atomic(&self.path(path), data)
    }

    fn remove(&self, path: &str) -> Result<()> {
        let full = self.path(path);
        match fs::remove_file(&full) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e).with_context(|| format!("удаление {}", full.display())),
            _ => Ok(()),
        }
    }

    fn list(&self, dir: &str) -> Result<Vec<String>> {
        let full = self.path(dir);
        let entries = match fs::read_dir(&full) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("чтение {}", full.display())),
        };
        Ok(entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect())
    }

    fn stamp(&self, path: &str) -> Option<Stamp> {
        let meta = fs::metadata(self.path(path)).ok()?;
        Some((meta.modified().ok()?, meta.len(), meta.ino()))
    }
}

/// Книжка в памяти — для тестов: отпечаток — номер записи.
#[cfg(test)]
#[derive(Default)]
pub struct MemStore {
    files: std::cell::RefCell<std::collections::BTreeMap<String, (Vec<u8>, u64)>>,
    version: std::cell::Cell<u64>,
}

#[cfg(test)]
impl Store for MemStore {
    fn read(&self, path: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.files.borrow().get(path).map(|(data, _)| data.clone()))
    }

    fn write(&self, path: &str, data: &[u8]) -> Result<()> {
        self.version.set(self.version.get() + 1);
        self.files.borrow_mut().insert(path.to_string(), (data.to_vec(), self.version.get()));
        Ok(())
    }

    fn remove(&self, path: &str) -> Result<()> {
        self.files.borrow_mut().remove(path);
        Ok(())
    }

    fn list(&self, dir: &str) -> Result<Vec<String>> {
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        Ok(self
            .files
            .borrow()
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix))
            .filter(|rest| !rest.contains('/'))
            .map(str::to_string)
            .collect())
    }

    fn stamp(&self, path: &str) -> Option<Stamp> {
        let files = self.files.borrow();
        let (data, version) = files.get(path)?;
        Some((SystemTime::UNIX_EPOCH, data.len() as u64, *version))
    }
}
