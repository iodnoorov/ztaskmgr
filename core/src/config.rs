//! Настройки этого устройства: `~/.config/ztaskmgr/config`. В книжку не входят и не
//! синхронизируются — на телефоне и компьютере они свои, а пароль WebDAV не должен уезжать
//! вместе с книжкой. Формат — строки `ключ = значение`, `#` — комментарий. Файл можно править
//! руками: при записи меняется только строка нужного ключа, остальное (комментарии, ключи
//! из будущих версий) остаётся как было.

use std::fs;
#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;

const HEADER: &str = "# ztaskmgr: настройки этого устройства (не синхронизируются)";

/// Ключи файла.
const PREVIEW: &str = "preview";
pub const WEBDAV_URL: &str = "webdav_url";
pub const WEBDAV_USER: &str = "webdav_user";
pub const WEBDAV_PASSWORD: &str = "webdav_password";
pub const WEBDAV_PASSWORD_COMMAND: &str = "webdav_password_command";
const WEBDAV_INSECURE: &str = "webdav_insecure";
pub const WEBDAV_PASSPHRASE: &str = "webdav_passphrase";
pub const WEBDAV_PASSPHRASE_COMMAND: &str = "webdav_passphrase_command";
pub const WEBDAV_CA: &str = "webdav_ca";

#[derive(Debug, Default)]
pub struct Config {
    /// Куда сохранять; `None` — только в памяти (тесты).
    path: Option<PathBuf>,
    lines: Vec<String>,
}

/// Значение строки `ключ = значение`, если ключ тот.
fn value_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    let (k, v) = line.split_once('=')?;
    (k.trim() == key).then(|| v.trim())
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_lowercase().as_str() {
        "on" | "yes" | "true" | "1" => Some(true),
        "off" | "no" | "false" | "0" => Some(false),
        _ => None,
    }
}

impl Config {
    /// `~/.config/ztaskmgr/config`.
    pub fn default_path() -> Option<PathBuf> {
        ProjectDirs::from("", "", "ztaskmgr").map(|dirs| dirs.config_dir().join("config"))
    }

    /// Нет файла — настройки по умолчанию, файл появится при первом изменении.
    pub fn load(path: PathBuf) -> Result<Self> {
        let lines = match fs::read_to_string(&path) {
            Ok(text) => text.lines().map(str::to_string).collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("чтение {}", path.display())),
        };
        Ok(Self { path: Some(path), lines })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.lines.iter().rev().find_map(|line| value_of(line, key))
    }

    /// Меняет строку ключа (последнюю, она и действует) или дописывает её, и сохраняет.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let previous_len = self.lines.len();
        let line = format!("{key} = {value}");
        let previous = match self.lines.iter().rposition(|l| value_of(l, key).is_some()) {
            Some(i) => Some((i, std::mem::replace(&mut self.lines[i], line))),
            None => {
                if self.lines.is_empty() {
                    self.lines.push(HEADER.into());
                }
                self.lines.push(line);
                None
            }
        };
        if let Err(error) = self.save() {
            match previous {
                Some((i, line)) => self.lines[i] = line,
                None => self.lines.truncate(previous_len),
            }
            return Err(error);
        }
        Ok(())
    }

    /// Пишет через временный файл, доступный только владельцу: здесь будут пароль и фраза.
    fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut text = self.lines.join("\n");
        text.push('\n');
        crate::store::write_atomic_mode(path, text.as_bytes(), 0o600)
    }

    /// Панель превью задачи справа (на широком экране). По умолчанию включена.
    pub fn preview(&self) -> bool {
        self.get(PREVIEW).and_then(parse_bool).unwrap_or(true)
    }

    pub fn set_preview(&mut self, on: bool) -> Result<()> {
        self.set(PREVIEW, if on { "on" } else { "off" })
    }

    /// WebDAV — если задан адрес.
    pub fn webdav(&self) -> Option<crate::webdav::Settings> {
        let get = |key| self.get(key).unwrap_or("").to_string();
        let url = get(WEBDAV_URL);
        (!url.is_empty()).then(|| crate::webdav::Settings {
            url,
            user: get(WEBDAV_USER),
            password: get(WEBDAV_PASSWORD),
            password_command: get(WEBDAV_PASSWORD_COMMAND),
            insecure: self.get(WEBDAV_INSECURE).and_then(parse_bool).unwrap_or(false),
            passphrase: get(WEBDAV_PASSPHRASE),
            passphrase_command: get(WEBDAV_PASSPHRASE_COMMAND),
            ca: get(WEBDAV_CA),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_other_lines() {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-config-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("sub/config");

        let mut config = Config::load(path.clone()).unwrap();
        assert!(config.preview());
        config.set_preview(false).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, format!("{HEADER}\npreview = off\n"));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

        // Руками: комментарий, чужой ключ, кривое значение — по умолчанию.
        fs::write(&path, "# мой\nfuture = 1\npreview=maybe\n").unwrap();
        let mut config = Config::load(path.clone()).unwrap();
        assert!(config.preview());
        config.set_preview(false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "# мой\nfuture = 1\npreview = off\n");
        assert!(!Config::load(path).unwrap().preview());
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn failed_set_restores_memory_and_keeps_existing_destination() {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-config-rollback-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("config");
        let mut config = Config::load(path.clone()).unwrap();
        config.set(WEBDAV_PASSWORD, "old secret").unwrap();
        let previous = config.lines.clone();
        // rename файла поверх каталога обязан отказать даже при запуске от root.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("preserved"), "old destination").unwrap();
        assert!(config.set(WEBDAV_PASSWORD, "new secret").is_err());
        assert_eq!(config.get(WEBDAV_PASSWORD), Some("old secret"));
        assert_eq!(config.lines, previous);
        assert_eq!(fs::read_to_string(path.join("preserved")).unwrap(), "old destination");
        fs::remove_dir_all(&path).unwrap();
        config.set(WEBDAV_USER, "user").unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("old secret") && !saved.contains("new secret"));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_config_commits_have_private_unique_temporaries() {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-config-concurrent-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("config");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|writer| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut config = Config::load(path).unwrap();
                    barrier.wait();
                    config.set(WEBDAV_PASSWORD, &format!("secret-{writer}")).unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let config = Config::load(path.clone()).unwrap();
        let value = config.get(WEBDAV_PASSWORD).unwrap();
        assert!((0..8).any(|writer| value == format!("secret-{writer}")));
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
