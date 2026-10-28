//! Настройки этого устройства: `~/.config/ztaskmgr/config`. В книжку не входят и не
//! синхронизируются — на телефоне и компьютере они свои, а пароль WebDAV не должен уезжать
//! вместе с книжкой. Формат — строки `ключ = значение`, `#` — комментарий. Файл можно править
//! руками: при записи меняется только строка нужного ключа, остальное (комментарии, ключи
//! из будущих версий) остаётся как было.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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
        let line = format!("{key} = {value}");
        match self.lines.iter().rposition(|l| value_of(l, key).is_some()) {
            Some(i) => self.lines[i] = line,
            None => {
                if self.lines.is_empty() {
                    self.lines.push(HEADER.into());
                }
                self.lines.push(line);
            }
        }
        self.save()
    }

    /// Пишет через временный файл, доступный только владельцу: здесь будет и пароль.
    fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("создание {}", dir.display()))?;
        }
        let mut text = self.lines.join("\n");
        text.push('\n');
        let mut tmp_name = path.as_os_str().to_owned();
        tmp_name.push(".tmp");
        let tmp = Path::new(&tmp_name);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(tmp)
            .with_context(|| format!("запись {}", tmp.display()))?;
        // Файл мог остаться от прошлого раза с другими правами.
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(tmp, path).with_context(|| format!("переименование в {}", path.display()))
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
}
