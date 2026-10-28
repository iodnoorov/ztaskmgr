//! Окно настроек устройства и состояние синхронизации с WebDAV.

use super::*;

/// Пункт окна настроек.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    Preview,
    WebdavUrl,
    WebdavUser,
    WebdavPassword,
    WebdavPasswordCommand,
    WebdavPassphrase,
    WebdavPassphraseCommand,
    ChangePassphrase,
    SyncNow,
}

impl Setting {
    pub const ALL: [Setting; 9] = [
        Setting::Preview,
        Setting::WebdavUrl,
        Setting::WebdavUser,
        Setting::WebdavPassword,
        Setting::WebdavPasswordCommand,
        Setting::WebdavPassphrase,
        Setting::WebdavPassphraseCommand,
        Setting::ChangePassphrase,
        Setting::SyncNow,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Setting::Preview => "Превью задачи справа",
            Setting::WebdavUrl => "WebDAV: адрес",
            Setting::WebdavUser => "WebDAV: логин",
            Setting::WebdavPassword => "WebDAV: пароль",
            Setting::WebdavPasswordCommand => "WebDAV: команда пароля",
            Setting::WebdavPassphrase => "Шифрование: фраза",
            Setting::WebdavPassphraseCommand => "Шифрование: команда фразы",
            Setting::ChangePassphrase => "Шифрование: сменить фразу",
            Setting::SyncNow => "Синхронизировать сейчас",
        }
    }

    pub fn help(self) -> &'static str {
        match self {
            Setting::Preview => "на экране от 120 колонок · v — переключить",
            Setting::WebdavUrl => "каталог книжки на сервере; пусто — WebDAV выключен",
            Setting::WebdavUser => "пусто — без входа",
            Setting::WebdavPassword => "лежит в файле настроек, доступном только вам",
            Setting::WebdavPasswordCommand => "например «pass ztaskmgr»; если задана — вместо пароля",
            Setting::WebdavPassphrase => "шифрует книжку на сервере; одна на всех устройствах",
            Setting::WebdavPassphraseCommand => "например «pass ztaskmgr-book»; если задана — вместо фразы",
            Setting::ChangePassphrase => "на сервере; на других устройствах потом — новую фразу выше",
            Setting::SyncNow => "иначе — раз в минуту и через пару секунд после правки",
        }
    }

    /// Ключ в файле настроек у текстовых пунктов.
    pub fn key(self) -> Option<&'static str> {
        match self {
            Setting::WebdavUrl => Some(crate::config::WEBDAV_URL),
            Setting::WebdavUser => Some(crate::config::WEBDAV_USER),
            Setting::WebdavPassword => Some(crate::config::WEBDAV_PASSWORD),
            Setting::WebdavPasswordCommand => Some(crate::config::WEBDAV_PASSWORD_COMMAND),
            Setting::WebdavPassphrase => Some(crate::config::WEBDAV_PASSPHRASE),
            Setting::WebdavPassphraseCommand => Some(crate::config::WEBDAV_PASSPHRASE_COMMAND),
            Setting::Preview | Setting::ChangePassphrase | Setting::SyncNow => None,
        }
    }

    pub fn is_on(self, config: &Config) -> bool {
        match self {
            Setting::Preview => config.preview(),
            _ => false,
        }
    }

    /// Значение текстового пункта для показа: пароль — точками, пустое — прочерком.
    pub fn value(self, config: &Config) -> Option<String> {
        let value = config.get(self.key()?).unwrap_or("");
        Some(match self {
            _ if value.is_empty() => "—".into(),
            Setting::WebdavPassword | Setting::WebdavPassphrase => "•".repeat(value.chars().count().min(12)),
            _ => value.to_string(),
        })
    }
}

/// Состояние синхронизации по WebDAV — для шапки.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SyncState {
    #[default]
    Off,
    /// Последняя удалась, во столько-то.
    Ok(String),
    Error(String),
}

impl App {
    pub(super) fn on_key_settings(&mut self, press: Press) {
        let Mode::Settings { row, input } = &mut self.mode else {
            return;
        };
        let setting = Setting::ALL[*row];
        if let Some(field) = input {
            let Some(code) = edit_line(field, &press) else {
                return;
            };
            let value = field.text().trim().to_string();
            *input = None;
            if (code, setting) == (KeyCode::Enter, Setting::ChangePassphrase) {
                if value.is_empty() {
                    self.status = Some(Status::Error("Новая фраза пустая — не сменена".into()));
                } else {
                    self.passphrase_change = Some(value);
                    self.status = Some(Status::Info("Шифрование: смена фразы…".into()));
                }
            } else if let (KeyCode::Enter, Some(key)) = (code, setting.key()) {
                match self.config.set(key, &value) {
                    // Главный цикл передаст новые настройки синхронизации.
                    Ok(()) => self.webdav_changed = true,
                    Err(e) => self.status = Some(Status::Error(format!("Настройки не сохранены: {e:#}"))),
                }
            }
            return;
        }
        let last = Setting::ALL.len() - 1;
        match press.hotkey.code {
            KeyCode::Char('j') | KeyCode::Down => *row = (*row + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => *row = row.saturating_sub(1),
            KeyCode::Char(' ' | 'x') | KeyCode::Enter => match setting {
                Setting::Preview => self.toggle_setting(setting),
                Setting::SyncNow if self.config.webdav().is_none() => {
                    self.status = Some(Status::Info("Сначала укажите адрес WebDAV".into()));
                }
                Setting::SyncNow => {
                    self.sync_requested = true;
                    self.status = Some(Status::Info("WebDAV: синхронизация…".into()));
                }
                Setting::ChangePassphrase if !self.config.webdav().is_some_and(|s| s.is_encrypted()) => {
                    self.status = Some(Status::Info("Шифрование не включено: сначала задайте фразу".into()));
                }
                Setting::ChangePassphrase => *input = Some(LineInput::new(String::new())),
                _ => {
                    let current = setting.key().and_then(|k| self.config.get(k)).unwrap_or("");
                    *input = Some(LineInput::new(current.to_string()));
                }
            },
            KeyCode::Esc | KeyCode::Char('q' | 'S') => self.mode = Mode::Normal,
            _ => {}
        }
    }

    pub(super) fn toggle_setting(&mut self, setting: Setting) {
        let on = !setting.is_on(&self.config);
        let saved = match setting {
            Setting::Preview => self.config.set_preview(on),
            _ => Ok(()),
        };
        if let Err(e) = saved {
            self.status = Some(Status::Error(format!("Настройки не сохранены: {e:#}")));
        }
    }

    /// Чем кончилась синхронизация в фоне. Ошибка — в строку состояния, но одна и та же —
    /// один раз, а не каждую минуту.
    pub fn on_sync(&mut self, outcome: crate::webdav::Outcome) {
        match outcome {
            Ok(summary) => {
                // С сервера пришли дела вчерашнего дня — они переезжают на сегодня, как
                // при запуске (перенесённое повторно не переносится).
                if summary.downloaded + summary.merged > 0 {
                    match self.book.carry_over(self.today) {
                        Ok(Some((from, count))) => {
                            self.status = Some(Status::Info(carried_message(from, count)));
                            self.edited = true;
                        }
                        Ok(None) => {}
                        Err(e) => self.status = Some(Status::Error(format!("Автоперенос: {e:#}"))),
                    }
                }
                // Файлы, которые не удалось синхронизировать, — ошибка, пока их не исправят.
                if !summary.failed.is_empty() {
                    let e = format!("не синхронизированы: {}", summary.failed.join("; "));
                    if self.sync != SyncState::Error(e.clone()) {
                        self.status = Some(Status::Error(format!("WebDAV: {e}")));
                    }
                    self.sync = SyncState::Error(e);
                    self.check_external();
                    return;
                }
                if matches!(self.sync, SyncState::Error(_)) {
                    self.status = Some(Status::Info("WebDAV: снова на связи".into()));
                } else if matches!(&self.status, Some(Status::Info(s)) if s.starts_with("WebDAV")) {
                    self.status = Some(Status::Info("WebDAV: синхронизировано".into()));
                }
                self.sync = SyncState::Ok(clock::hm());
                // Пришедшее с сервера — на экран.
                self.check_external();
            }
            Err(e) => {
                if self.sync != SyncState::Error(e.clone()) {
                    self.status = Some(Status::Error(format!("WebDAV: {e}")));
                }
                self.sync = SyncState::Error(e);
            }
        }
    }

    /// Чем кончилась смена фразы. Новая фраза — в настройки, если фраза хранится там; если её
    /// печатает команда — обновить то, откуда она её берёт, может только пользователь.
    pub fn on_passphrase(&mut self, changed: crate::webdav::Changed) {
        self.status = Some(match changed {
            Err(e) => Status::Error(format!("Фраза не сменена: {e}")),
            Ok(_) if self.config.get(crate::config::WEBDAV_PASSPHRASE_COMMAND).is_some_and(|c| !c.trim().is_empty()) => {
                Status::Info(
                    "Фраза сменена на сервере. Обновите то, что печатает команда фразы, и введите новую фразу на других устройствах"
                        .into(),
                )
            }
            Ok(new) => match self.config.set(crate::config::WEBDAV_PASSPHRASE, &new) {
                Ok(()) => Status::Info("Фраза сменена — введите новую фразу на других устройствах".into()),
                Err(e) => Status::Error(format!("Фраза сменена на сервере, но не сохранена в настройках ({e:#}) — впишите её")),
            },
        });
    }

    /// `v`: превью видно только на широком экране — на узком говорим об этом.
    pub(super) fn toggle_preview(&mut self) {
        self.toggle_setting(Setting::Preview);
        if self.status.is_none() {
            let what = if self.config.preview() { "включено" } else { "выключено" };
            let note = if self.config.preview() && self.width < crate::ui::PREVIEW_MIN {
                format!(" (видно на экране от {} колонок)", crate::ui::PREVIEW_MIN)
            } else {
                String::new()
            };
            self.status = Some(Status::Info(format!("Превью {what}{note}")));
        }
    }
}
