//! Где лежат файлы книжки. `Book` работает с ними только через `Store` — по относительному
//! пути (`2026-10-26.md`, `notes/k3x9ab.md`), не зная, файловая это система или что-то другое.
//! Сейчас это каталог на диске (`FsStore`); в тестах — память (`MemStore`). Так же сюда
//! встанет, например, шифрование книжки — обёрткой над другим `Store`.

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Межпроцессная блокировка транзакции; хранилища в памяти не требуют файлов.
    fn lock(&self) -> Result<Option<fs::File>> {
        Ok(None)
    }
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
    write_atomic_mode(path, data, 0o666)
}

/// Уникальная заготовка в том же каталоге; права задаются при create_new, до записи.
pub(crate) fn write_atomic_mode(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).with_context(|| format!("создание {}", dir.display()))?;
    }
    let (tmp, mut file) = loop {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".tmp-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let tmp = PathBuf::from(name);
        match fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp) {
            Ok(file) => break (tmp, file),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("запись {}", tmp.display())),
        }
    };
    let result = (|| {
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path).with_context(|| format!("переименование в {}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}

impl Store for FsStore {
    fn lock(&self) -> Result<Option<fs::File>> {
        fs::create_dir_all(&self.dir).with_context(|| format!("создание {}", self.dir.display()))?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(".book-lock"))
            .context("блокировка книжки")?;
        // SAFETY: flock получает действующий файловый дескриптор и не обращается к памяти.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error()).context("блокировка книжки");
        }
        Ok(Some(file))
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_atomic_writes_never_share_a_temporary_inode() {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-atomic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("page.md");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0u8..8)
            .map(|writer| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let data = vec![writer; 512 * 1024];
                    barrier.wait();
                    write_atomic(&path, &data).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let data = fs::read(&path).unwrap();
        assert_eq!(data.len(), 512 * 1024);
        assert!(data.iter().all(|byte| *byte == data[0]));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn interprocess_atomic_writes_use_unique_temporaries() {
        const CHILD: &str = "ZTASKMGR_ATOMIC_TEST_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let byte: u8 = std::env::var("ZTASKMGR_ATOMIC_TEST_BYTE").unwrap().parse().unwrap();
            for _ in 0..4 {
                write_atomic(Path::new(&path), &vec![byte; 256 * 1024]).unwrap();
            }
            return;
        }
        let dir = std::env::temp_dir().join(format!("ztaskmgr-atomic-process-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("page.md");
        let children: Vec<_> = (0..4)
            .map(|writer| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "store::tests::interprocess_atomic_writes_use_unique_temporaries"])
                    .env(CHILD, &path)
                    .env("ZTASKMGR_ATOMIC_TEST_BYTE", writer.to_string())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        }
        let data = fs::read(path).unwrap();
        assert_eq!(data.len(), 256 * 1024);
        assert!(data.iter().all(|byte| *byte == data[0]));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
