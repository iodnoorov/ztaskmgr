//! Синхронизация книжки с WebDAV-сервером пользователя (Nextcloud, ownCloud, Apache, rclone…).
//!
//! Пофайлово: страницы, записи задач (`notes/`), `LISTS.md`, `DELETED_NOTES.md`. Что было
//! на сервере и у нас при прошлой синхронизации, помнит манифест `.webdav-state` (ETag сервера
//! и хеш содержимого по каждому файлу). По нему видно, кто что менял:
//!
//! - изменилось только у нас — PUT (с `If-Match`, чтобы не затереть чужую запись), удалили
//!   у нас — DELETE;
//! - только на сервере — GET, удалили там — удаляем у себя;
//! - с обеих сторон — слияние (`merge.rs`) и PUT результата.
//!
//! Сервер ответил «412: файл уже другой» — файл пропускается, в следующий раз он сольётся.
//! Несколько процессов (программа и MCP-сервер) не синхронизируются одновременно —
//! `.webdav-lock`.
//!
//! С фразой шифрования файлы на сервере зашифрованы (`crypt.rs`); всё остальное — манифест,
//! слияние — работает с открытым текстом, как без шифрования. Незашифрованный файл на сервере
//! (книжка до шифрования) читается как есть; безопасная перезапись требует сильного ETag.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::net::IpAddr;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::book::{Book, file_kind};
use crate::crypt::{self, Cipher, KEY_FILE, Opened};
use crate::store::write_atomic;

const STATE_FILE: &str = ".webdav-state";
const LOCK_FILE: &str = ".webdav-lock";

/// Сколько ждать сервер, прежде чем сдаться.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Как часто синхронизироваться, пока программа открыта.
pub const EVERY: Duration = Duration::from_secs(60);

/// Настройки подключения (из `config`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// Каталог книжки на сервере, например `https://cloud.example.com/remote.php/dav/files/me/ztaskmgr/`.
    pub url: String,
    pub user: String,
    pub password: String,
    /// Команда, которая печатает пароль (`pass ztaskmgr`); если задана — главнее `password`.
    pub password_command: String,
    /// Разрешить пароль по `http://` не на этот же компьютер (`webdav_insecure = on`).
    pub insecure: bool,
    /// Фраза шифрования книжки на сервере; пусто — не шифровать.
    pub passphrase: String,
    /// Команда, которая печатает фразу; если задана — главнее `passphrase`.
    pub passphrase_command: String,
    /// PEM-файл своего центра сертификации (`webdav_ca`): ему доверяем вдобавок к встроенным
    /// и системным, в том числе в Termux без доступа к системному хранилищу.
    pub ca: String,
}

/// Первая строка вывода команды (`pass ztaskmgr`): пароль или фраза.
fn run_command(command: &str, what: &str) -> Result<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .output()
        .with_context(|| format!("запуск «{command}»"))?;
    if !out.status.success() {
        bail!("команда {what} «{command}» завершилась с ошибкой");
    }
    let text = String::from_utf8(out.stdout).with_context(|| format!("команда {what} вывела не текст"))?;
    Ok(text.lines().next().unwrap_or("").to_string())
}

/// Числовой loopback или ровно `localhost` (без поддоменов и завершающей точки).
/// DNS-имена с похожим префиксом не получают исключение из требования HTTPS.
fn is_loopback(url: &str) -> bool {
    let Ok(uri) = url.parse::<ureq::http::Uri>() else {
        return false;
    };
    let Some(host) = uri.host() else {
        return false;
    };
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    host.eq_ignore_ascii_case("localhost") || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Только сильный ETag пригоден для атомарного изменения существующего файла.
fn strong_etag(etag: Option<&str>) -> Result<&str> {
    let Some(etag) = etag else {
        bail!("сервер не сообщил сильный ETag — безопасная перезапись/удаление отложены");
    };
    if !etag.starts_with('"')
        || !etag.ends_with('"')
        || etag.len() < 2
        || etag.as_bytes()[1..etag.len() - 1].iter().any(|&b| b == b'"' || b < 0x21 || b == 0x7f)
    {
        bail!("сервер не сообщил сильный ETag ({etag}) — безопасная перезапись/удаление отложены");
    }
    Ok(etag)
}

impl Settings {
    /// Пароль и фраза из команд — один раз, до синхронизаций: команда (`pass`, `gpg`) может спросить
    /// пароль у пользователя, а в фоне, поверх интерфейса, это сломало бы экран. Поэтому её
    /// запускают, когда терминал свободен (при запуске, при смене настроек), а потоку
    /// синхронизации достаются готовые настройки. Ввод команды — `/dev/null`: у MCP-сервера
    /// stdin занят протоколом, а pinentry и так работает с `/dev/tty`.
    pub fn resolve(mut self) -> Result<Self> {
        if !self.password_command.trim().is_empty() {
            self.password = run_command(&self.password_command, "пароля")?;
            self.password_command.clear();
        }
        if !self.passphrase_command.trim().is_empty() {
            self.passphrase = run_command(&self.passphrase_command, "фразы")?;
            if self.passphrase.is_empty() {
                bail!("команда фразы «{}» ничего не вывела", self.passphrase_command);
            }
            self.passphrase_command.clear();
        }
        Ok(self)
    }

    /// Шифруется ли книжка: задана фраза или команда фразы.
    pub fn is_encrypted(&self) -> bool {
        !self.passphrase.is_empty() || !self.passphrase_command.trim().is_empty()
    }

    /// Нужна ли команда, чтобы получить пароль или фразу (то есть свободный терминал).
    pub fn needs_command(&self) -> bool {
        !self.password_command.trim().is_empty() || !self.passphrase_command.trim().is_empty()
    }
}

// ------------------------------------------------------------------ сервер

/// Ответ на PUT.
#[derive(Debug, PartialEq, Eq)]
enum Put {
    /// Записано; ETag, если сервер его сообщил.
    Done(Option<String>),
    /// Файл на сервере уже не тот, что мы видели (412).
    Changed,
}

/// Файл в каталоге на сервере: имя и ETag.
type Listed = (String, Option<String>);

struct Remote {
    agent: ureq::Agent,
    /// Адрес каталога, со `/` на конце.
    base: String,
    auth: Option<String>,
    /// Ключ книжки, если она шифруется (узнаётся в начале синхронизации).
    cipher: Option<Cipher>,
}

/// Корневые сертификаты для https: встроенные (Mozilla), системные — так подходят и
/// свои центры сертификации, добавленные в систему (`trust anchor`, `update-ca-certificates`), —
/// и `extra` из настроек. Системное хранилище читается один раз за запуск.
fn root_certs(extra: Vec<ureq::tls::Certificate<'static>>) -> ureq::tls::RootCerts {
    static ROOTS: OnceLock<Arc<Vec<ureq::tls::Certificate<'static>>>> = OnceLock::new();
    let roots = ROOTS.get_or_init(|| {
        let system = rustls_native_certs::load_native_certs().certs;
        let bundled = webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter();
        Arc::new(
            bundled
                .map(|c| ureq::tls::Certificate::from_der(c).to_owned())
                .chain(system.iter().map(|c| ureq::tls::Certificate::from_der(c).to_owned()))
                .collect(),
        )
    });
    if extra.is_empty() {
        return ureq::tls::RootCerts::Specific(roots.clone());
    }
    ureq::tls::RootCerts::Specific(Arc::new(roots.iter().cloned().chain(extra).collect()))
}

/// Сертификаты из PEM (`-----BEGIN CERTIFICATE-----`); ни одного — ошибка.
pub fn parse_ca(pem: &[u8]) -> Result<Vec<ureq::tls::Certificate<'static>>> {
    let mut certs = Vec::new();
    for item in ureq::tls::parse_pem(pem) {
        if let ureq::tls::PemItem::Certificate(cert) = item.context("не PEM")? {
            certs.push(cert);
        }
    }
    if certs.is_empty() {
        bail!("в нём нет сертификата (-----BEGIN CERTIFICATE-----)");
    }
    Ok(certs)
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ABC[(n >> (18 - 6 * i)) as usize & 63] as char } else { '=' });
        }
    }
    out
}

impl Remote {
    fn new(settings: &Settings) -> Result<Self> {
        let url = settings.url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            bail!("адрес WebDAV должен начинаться с http:// или https://");
        }
        let base = if url.ends_with('/') { url.to_string() } else { format!("{url}/") };
        if settings.needs_command() {
            bail!("пароль или фраза не получены: команда не запускалась");
        }
        // Basic-авторизация — это пароль открытым текстом: по сети — только через https.
        if url.starts_with("http://") && !settings.user.is_empty() && !settings.insecure && !is_loopback(url) {
            bail!(
                "по http:// пароль уйдёт по сети открытым текстом — нужен https:// (или webdav_insecure = on в настройках, если сеть ваша)"
            );
        }
        let auth = (!settings.user.is_empty())
            .then(|| format!("Basic {}", base64(format!("{}:{}", settings.user, settings.password).as_bytes())));
        let extra = match settings.ca.trim() {
            "" => Vec::new(),
            path => {
                // Путь пишут руками — `~/` раскрываем, как это сделала бы оболочка.
                let file = match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
                    (Some(rest), Some(home)) => Path::new(&home).join(rest),
                    _ => path.into(),
                };
                let pem = fs::read(file).with_context(|| format!("сертификат webdav_ca «{path}»"))?;
                parse_ca(&pem).with_context(|| format!("сертификат webdav_ca «{path}»"))?
            }
        };
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .allow_non_standard_methods(true)
            .timeout_global(Some(TIMEOUT))
            .tls_config(ureq::tls::TlsConfig::builder().root_certs(root_certs(extra)).build())
            .build()
            .into();
        Ok(Self { agent, base, auth, cipher: None })
    }

    /// Запрос к файлу или каталогу `path` (относительно `base`): код ответа, ETag и тело.
    fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<(u16, Option<String>, Vec<u8>)> {
        let mut request = ureq::http::Request::builder().method(method).uri(format!("{}{path}", self.base));
        if let Some(auth) = &self.auth {
            request = request.header("Authorization", auth);
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request.body(body.to_vec()).context("запрос WebDAV")?;
        let mut response = self.agent.run(request).with_context(|| format!("WebDAV: {method} {}{path}", self.base))?;
        let status = response.status().as_u16();
        let etag = response.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_string);
        let body = response.body_mut().with_config().limit(64 * 1024 * 1024).read_to_vec()?;
        match status {
            401 | 403 => bail!("WebDAV: доступ запрещён (HTTP {status}) — проверьте логин и пароль"),
            _ => Ok((status, etag, body)),
        }
    }

    fn fail<T>(method: &str, path: &str, status: u16) -> Result<T> {
        bail!("WebDAV: {method} {path}: HTTP {status}")
    }

    /// Файлы каталога `dir` (`""` или `"notes/"`) с ETag; нет каталога — `None`.
    fn list(&self, dir: &str) -> Result<Option<Vec<Listed>>> {
        let body = br#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:getetag/><d:resourcetype/></d:prop></d:propfind>"#;
        let headers = [("Depth", "1"), ("Content-Type", "application/xml; charset=utf-8")];
        let (status, _, body) = self.call("PROPFIND", dir, &headers, body)?;
        match status {
            404 => Ok(None),
            207 => {
                let xml = String::from_utf8_lossy(&body);
                Ok(Some(
                    parse_multistatus(&xml)
                        .into_iter()
                        .filter(|e| !e.collection)
                        .filter_map(|e| Some((file_name(&e.href)?, e.etag)))
                        .collect(),
                ))
            }
            _ => Self::fail("PROPFIND", dir, status),
        }
    }

    fn mkcol(&self, dir: &str) -> Result<()> {
        let (status, _, _) = self.call("MKCOL", dir, &[], b"")?;
        // 405 — каталог уже есть.
        match status {
            200..=299 | 405 => Ok(()),
            _ => Self::fail("MKCOL", dir, status),
        }
    }

    /// Содержимое (как на сервере, зашифрованное) и ETag; файла нет — `None`.
    fn get(&self, path: &str) -> Result<Option<(Vec<u8>, Option<String>)>> {
        let (status, etag, body) = self.call("GET", path, &[], b"")?;
        match status {
            200 => Ok(Some((body, etag))),
            404 => Ok(None),
            _ => Self::fail("GET", path, status),
        }
    }

    /// Открытый текст файла с сервера и `true`, если файл там не в том виде, в каком должен быть
    /// (не зашифрован, хотя книжка шифруется), — его надо перезаписать.
    fn open(&self, path: &str, data: &[u8]) -> Result<(Vec<u8>, bool)> {
        match &self.cipher {
            Some(cipher) => Ok(match cipher.open(path, data)? {
                Opened::Encrypted(data) => (data, false),
                Opened::Plain(data) => (data, true),
            }),
            None if crypt::is_sealed(data) => bail!("зашифрован на сервере, а фразы шифрования нет"),
            None => Ok((data.to_vec(), false)),
        }
    }

    /// Запись открытого текста `data` (шифруется, если книжка шифруется), если на сервере всё
    /// ещё `expected` (`None` — файла там быть не должно).
    fn put(&self, path: &str, data: &[u8], expected: Option<&str>) -> Result<Put> {
        let condition = match expected {
            Some(etag) => ("If-Match", strong_etag(Some(etag))?),
            None => ("If-None-Match", "*"),
        };
        let (body, kind) = match &self.cipher {
            Some(cipher) => (cipher.seal(path, data)?, "application/octet-stream"),
            None => (data.to_vec(), "text/markdown; charset=utf-8"),
        };
        let headers = [("Content-Type", kind), condition];
        let (status, etag, _) = self.call("PUT", path, &headers, &body)?;
        match status {
            200..=299 => Ok(Put::Done(etag)),
            412 => Ok(Put::Changed),
            _ => Self::fail("PUT", path, status),
        }
    }

    /// Удаление только с сильным ETag; `false` — файл на сервере уже другой.
    fn delete(&self, path: &str, etag: &str) -> Result<bool> {
        let headers = [("If-Match", strong_etag(Some(etag))?)];
        let (status, _, _) = self.call("DELETE", path, &headers, b"")?;
        match status {
            200..=299 | 404 => Ok(true),
            412 => Ok(false),
            _ => Self::fail("DELETE", path, status),
        }
    }

    /// Перезапись существующего файла, в том числе миграция в зашифрованный вид.
    fn replace(&self, path: &str, data: &[u8], etag: Option<&str>) -> Result<Put> {
        self.put(path, data, Some(strong_etag(etag)?))
    }

    /// Ключ книжки: с сервера, а если его там нет — новый (книжка начинает шифроваться).
    /// Без фразы — `None`, но если книжка на сервере уже зашифрована — ошибка: иначе мы
    /// положили бы рядом с зашифрованными файлами открытые.
    fn book_key(&self, passphrase: &str, listed: bool, dirs: &mut (bool, bool)) -> Result<Option<Cipher>> {
        if passphrase.is_empty() {
            if listed {
                bail!("книжка на сервере зашифрована — укажите фразу шифрования в настройках (S)");
            }
            return Ok(None);
        }
        if !listed {
            ensure_dirs(self, KEY_FILE, dirs)?;
            let (cipher, file) = Cipher::create(passphrase)?;
            match self.put_key(&file, ("If-None-Match", "*"))? {
                Put::Done(_) => return Ok(Some(cipher)),
                // Только что создало другое устройство — берём его ключ.
                Put::Changed => {}
            }
        }
        let (file, _) = self.get(KEY_FILE)?.context("ключ книжки пропал с сервера во время синхронизации")?;
        Cipher::unlock(passphrase, &file).map(Some)
    }

    /// Запись `KEY_FILE` — как есть, он не шифруется ключом книжки.
    fn put_key(&self, file: &[u8], condition: (&str, &str)) -> Result<Put> {
        let headers = [("Content-Type", "application/json"), condition];
        let (status, etag, _) = self.call("PUT", KEY_FILE, &headers, file)?;
        match status {
            200..=299 => Ok(Put::Done(etag)),
            412 => Ok(Put::Changed),
            _ => Self::fail("PUT", KEY_FILE, status),
        }
    }
}

/// Элемент ответа PROPFIND.
#[derive(Debug, Default, PartialEq, Eq)]
struct Entry {
    href: String,
    etag: Option<String>,
    collection: bool,
}

/// XML-сущности: именованные и числовые (`&#34;`, `&#x22;` — так некоторые серверы
/// кодируют кавычки ETag). `&amp;` — последней, чтобы `&amp;quot;` стало `&quot;`.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("&#") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 2..];
        let decoded = tail.find(';').and_then(|end| {
            let code = &tail[..end];
            let n = match code.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => code.parse().ok()?,
            };
            Some((char::from_u32(n)?, end))
        });
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push_str("&#");
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// Разбор `multistatus` без XML-библиотеки: префиксы пространств имён у серверов разные
/// (`d:`, `D:`, без префикса), поэтому теги сравниваются по имени без префикса.
fn parse_multistatus(xml: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut current: Option<Entry> = None;
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        let Some(end) = rest[start..].find('>') else { break };
        let tag = &rest[start + 1..start + end];
        let after = &rest[start + end + 1..];
        let closing = tag.starts_with('/');
        let name = tag.trim_start_matches('/').split([' ', '/', '\t', '\n']).next().unwrap_or("");
        let local = name.rsplit(':').next().unwrap_or(name);
        let text = || unescape(after[..after.find('<').unwrap_or(after.len())].trim());
        match (local, closing) {
            ("response", false) => current = Some(Entry::default()),
            ("response", true) => entries.extend(current.take()),
            ("href", false) => {
                if let Some(e) = &mut current {
                    e.href = text();
                }
            }
            ("getetag", false) if !tag.ends_with('/') => {
                if let Some(e) = &mut current {
                    e.etag = Some(text()).filter(|t| !t.is_empty());
                }
            }
            ("collection", false) => {
                if let Some(e) = &mut current {
                    e.collection = true;
                }
            }
            _ => {}
        }
        rest = after;
    }
    entries
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(n) = text.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(n);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Имя файла из `href` (полный адрес или путь).
fn file_name(href: &str) -> Option<String> {
    let decoded = percent_decode(href);
    let name = decoded.trim_end_matches('/').rsplit('/').next()?;
    (!name.is_empty()).then(|| name.to_string())
}

// ------------------------------------------------------------------ синхронизация

/// Файлы книжки, которые возит WebDAV: путь относительно каталога книжки.
fn is_book_file(path: &str) -> bool {
    file_kind(path).is_some()
}

/// Хеш содержимого (FNV-1a): устойчив между версиями Rust, в отличие от `DefaultHasher`.
fn hash(data: &[u8]) -> String {
    let h = data.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    format!("{h:016x}")
}

/// Что было при прошлой синхронизации.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Known {
    /// ETag на сервере; `None` — сервер не сообщил, узнаем в следующий раз.
    etag: Option<String>,
    hash: String,
}

/// Манифест: для какого адреса и что было по каждому файлу. Другой адрес или книжка начала
/// (перестала) шифроваться — всё сначала: так каждый файл на сервере будет прочитан и, если он
/// не в том виде, перезаписан.
fn load_state(dir: &Path, url: &str, encrypted: bool) -> HashMap<String, Known> {
    let Ok(text) = fs::read_to_string(dir.join(STATE_FILE)) else {
        return HashMap::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return HashMap::new();
    };
    if value["url"].as_str() != Some(url) || value["encrypted"].as_bool().unwrap_or(false) != encrypted {
        return HashMap::new();
    }
    value["files"]
        .as_object()
        .map(|files| {
            files
                .iter()
                .filter_map(|(path, v)| {
                    let known =
                        Known { etag: v["etag"].as_str().map(str::to_string), hash: v["hash"].as_str()?.to_string() };
                    Some((path.clone(), known))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_state(dir: &Path, url: &str, encrypted: bool, state: &HashMap<String, Known>) -> Result<()> {
    let files: serde_json::Map<String, Value> =
        state.iter().map(|(path, k)| (path.clone(), json!({"etag": k.etag, "hash": k.hash}))).collect();
    let text = serde_json::to_string_pretty(&json!({"url": url, "encrypted": encrypted, "files": files}))?;
    write_atomic(&dir.join(STATE_FILE), text.as_bytes())
}

/// Файлы книжки у нас: путь → содержимое.
fn local_files(book: &Book) -> Result<HashMap<String, Vec<u8>>> {
    let mut files = HashMap::new();
    for path in book.files()? {
        if let Some(data) = book.read_file(&path)? {
            files.insert(path, data);
        }
    }
    Ok(files)
}

/// Что сделала синхронизация.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub uploaded: usize,
    pub downloaded: usize,
    pub merged: usize,
    pub deleted: usize,
    /// Отложены до следующего раза: файл поменялся, пока мы его синхронизировали.
    pub postponed: usize,
    /// Не синхронизированы из-за локальной ошибки или отсутствия безопасного ETag: «путь: причина».
    pub failed: Vec<String>,
}

/// Блокировка от одновременной синхронизации из двух процессов: держится, пока жив объект.
struct Lock {
    _file: fs::File,
}

impl Lock {
    /// `None` — синхронизирует другой процесс.
    fn try_take(dir: &Path) -> Result<Option<Self>> {
        fs::create_dir_all(dir).with_context(|| format!("создание {}", dir.display()))?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK_FILE))
            .context("файл блокировки WebDAV")?;
        // SAFETY: дескриптор открытого файла, flock не трогает память.
        let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        Ok(taken.then_some(Lock { _file: file }))
    }
}

/// Одна синхронизация книжки с сервером. `None` — сейчас синхронизирует другой процесс.
pub fn sync(book: &Book, settings: &Settings) -> Result<Option<Summary>> {
    let dir = book.dir();
    let Some(_lock) = Lock::try_take(dir)? else {
        return Ok(None);
    };
    let mut remote = Remote::new(settings)?;
    let mut summary = Summary::default();

    // Что на сервере. Нет каталога книжки — создадим при первой записи.
    let top = remote.list("")?;
    let notes = if top.is_some() { remote.list("notes/")? } else { None };
    let mut dirs_ready = (top.is_some(), notes.is_some());
    let has_key = top.iter().flatten().any(|(name, _)| name == KEY_FILE);
    remote.cipher = remote.book_key(&settings.passphrase, has_key, &mut dirs_ready)?;
    let encrypted = remote.cipher.is_some();
    let mut state = load_state(dir, &remote.base, encrypted);
    let mut server: HashMap<String, Option<String>> = HashMap::new();
    for (prefix, files) in [("", top), ("notes/", notes)] {
        for (name, etag) in files.unwrap_or_default() {
            let path = format!("{prefix}{name}");
            if is_book_file(&path) {
                server.insert(path, etag);
            }
        }
    }
    let local = {
        let _guard = book.lock()?;
        local_files(book)?
    };
    let paths: BTreeSet<&String> = local.keys().chain(server.keys()).chain(state.keys()).collect();
    let paths: Vec<String> = paths.into_iter().cloned().collect();

    // Ошибка одного файла (не текст, не разбирается, не записывается) не останавливает
    // остальные: файл — в `failed`, дальше. Ошибка сети или сервера прерывает проход, но то,
    // что уже сделано, остаётся в манифесте.
    let run = || -> Result<()> {
        macro_rules! local {
            ($path:expr, $result:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(e) => {
                        summary.failed.push(format!("{}: {e:#}", $path));
                        continue;
                    }
                }
            };
        }
        for path in paths {
            let mine = local.get(&path);
            let theirs = server.get(&path);
            let known = state.get(&path).cloned();
            let my_hash = mine.map(|d| hash(d));
            let local_changed = my_hash.as_ref() != known.as_ref().map(|k| &k.hash);
            let remote_changed = match (theirs, &known) {
                (None, None) => false,
                (Some(_), None) | (None, Some(_)) => true,
                (Some(etag), Some(k)) => etag.is_none() || *etag != k.etag,
            };
            // Пишем в файл, только если он всё ещё такой, каким мы его прочитали: программа
            // могла сохранить его, пока мы ходили на сервер.
            let unchanged_here = || -> Result<bool> { Ok(book.read_file(&path)?.as_ref().map(|d| hash(d)) == my_hash) };
            // Локальное удаление нельзя выдавать за выполненное без безопасного DELETE.
            if local_changed
                && mine.is_none()
                && known.is_some()
                && let Some(etag) = theirs
            {
                local!(path, strong_etag(etag.as_deref()));
            }

            match (local_changed, remote_changed, mine, theirs) {
                (false, false, ..) => {}
                // Только у нас.
                (true, false, Some(data), _) => {
                    ensure_dirs(&remote, &path, &mut dirs_ready)?;
                    let expected = match theirs {
                        Some(etag) => Some(local!(path, strong_etag(etag.as_deref()))),
                        None => None,
                    };
                    match remote.put(&path, data, expected)? {
                        Put::Done(etag) => {
                            state.insert(path, Known { etag, hash: my_hash.unwrap() });
                            summary.uploaded += 1;
                        }
                        Put::Changed => summary.postponed += 1,
                    }
                }
                (true, false, None, Some(_)) => {
                    let etag = local!(path, strong_etag(theirs.and_then(|e| e.as_deref())));
                    if remote.delete(&path, etag)? {
                        state.remove(&path);
                        summary.deleted += 1;
                    } else {
                        summary.postponed += 1;
                    }
                }
                (true, false, None, None) => {
                    state.remove(&path);
                }
                // Только на сервере (или с обеих сторон, но у нас файла уже нет — берём серверный:
                // удаление может «воскреснуть», зато ничего не теряется).
                (false, true, _, Some(_)) | (true, true, None, Some(_)) => match remote.get(&path)? {
                    Some((data, etag)) => {
                        let etag = etag.or_else(|| theirs.cloned().flatten());
                        let (data, stale) = local!(path, remote.open(&path, &data));
                        let new_hash = hash(&data);
                        {
                            let _guard = local!(path, book.lock());
                            if !local!(path, unchanged_here()) {
                                summary.postponed += 1;
                                continue;
                            }
                            if my_hash.as_ref() != Some(&new_hash) {
                                let text = local!(path, std::str::from_utf8(&data).context("не текст"));
                                local!(path, book.write_file(&path, text.as_bytes()));
                                summary.downloaded += 1;
                            }
                        }
                        let etag = if stale {
                            let expected = local!(path, strong_etag(etag.as_deref()));
                            match remote.replace(&path, &data, Some(expected))? {
                                Put::Done(etag) => etag,
                                Put::Changed => {
                                    summary.postponed += 1;
                                    continue;
                                }
                            }
                        } else {
                            etag
                        };
                        state.insert(path, Known { etag, hash: new_hash });
                    }
                    None => summary.postponed += 1,
                },
                (false, true, Some(_), None) => {
                    let _guard = local!(path, book.lock());
                    if local!(path, unchanged_here()) {
                        local!(path, book.remove_file(&path).context("удаление"));
                        state.remove(&path);
                        summary.deleted += 1;
                    } else {
                        summary.postponed += 1;
                    }
                }
                (false, true, None, None) => {
                    state.remove(&path);
                }
                // На сервере удалили, у нас меняли — наше снова на сервер.
                (true, true, Some(data), None) => {
                    ensure_dirs(&remote, &path, &mut dirs_ready)?;
                    match remote.put(&path, data, None)? {
                        Put::Done(etag) => {
                            state.insert(path, Known { etag, hash: my_hash.unwrap() });
                            summary.uploaded += 1;
                        }
                        Put::Changed => summary.postponed += 1,
                    }
                }
                // Меняли с обеих сторон — слияние.
                (true, true, Some(_), Some(_)) => {
                    let Some((data, etag)) = remote.get(&path)? else {
                        summary.postponed += 1;
                        continue;
                    };
                    let (data, stale) = local!(path, remote.open(&path, &data));
                    let their_hash = hash(&data);
                    let merged = {
                        let _guard = local!(path, book.lock());
                        if !local!(path, unchanged_here()) {
                            summary.postponed += 1;
                            continue;
                        }
                        if my_hash.as_ref() == Some(&their_hash) {
                            None
                        } else {
                            let text = local!(path, std::str::from_utf8(&data).context("не текст"));
                            local!(path, book.merge_text(&path, text).context("слияние"));
                            Some(local!(path, book.read_file(&path).context("чтение результата слияния")))
                        }
                    };
                    if my_hash.as_ref() == Some(&their_hash) {
                        let etag = if stale {
                            let expected = local!(path, strong_etag(etag.as_deref()));
                            match remote.replace(&path, &data, Some(expected))? {
                                Put::Done(etag) => etag,
                                Put::Changed => {
                                    summary.postponed += 1;
                                    continue;
                                }
                            }
                        } else {
                            etag
                        };
                        state.insert(path, Known { etag, hash: their_hash });
                        continue;
                    }
                    let merged = merged.expect("различающееся содержимое слито под локальной блокировкой");
                    let expected = local!(path, strong_etag(etag.as_deref()));
                    let result = match &merged {
                        Some(merged) => remote.put(&path, merged, Some(expected))?,
                        // Слияние дало пустую страницу — файла нет и на сервере.
                        None => {
                            if remote.delete(&path, expected)? {
                                Put::Done(None)
                            } else {
                                Put::Changed
                            }
                        }
                    };
                    match (result, merged) {
                        (Put::Done(etag), Some(merged)) => {
                            state.insert(path, Known { etag, hash: hash(&merged) });
                            summary.merged += 1;
                        }
                        (Put::Done(_), None) => {
                            state.remove(&path);
                            summary.merged += 1;
                        }
                        (Put::Changed, _) => summary.postponed += 1,
                    }
                }
                (true, true, None, None) => {
                    state.remove(&path);
                }
            }
        }
        Ok(())
    };
    let result = run();
    {
        let _guard = book.lock()?;
        save_state(dir, &remote.base, encrypted, &state)?;
    }
    result.map(|()| Some(summary))
}

/// Смена фразы шифрования: ключ книжки тот же, на сервере перешифровывается только
/// `KEY_FILE`, файлы книжки не трогаются. Другие устройства со старой фразой синхронизироваться
/// перестанут («неверная фраза»), пока в них не введут новую.
pub fn change_passphrase(settings: &Settings, new: &str) -> Result<()> {
    if settings.passphrase.is_empty() {
        bail!("шифрование не включено: сначала задайте фразу");
    }
    if new.is_empty() {
        bail!("новая фраза пустая");
    }
    let remote = Remote::new(settings)?;
    let (file, etag) =
        remote.get(KEY_FILE)?.context("книжка на сервере ещё не зашифрована — сначала синхронизируйтесь с фразой")?;
    let cipher = Cipher::unlock(&settings.passphrase, &file).context("текущая фраза")?;
    let wrapped = cipher.wrap(new)?;
    let etag = strong_etag(etag.as_deref()).context("смена фразы отложена")?;
    if remote.put_key(&wrapped, ("If-Match", etag))? == Put::Changed {
        bail!("ключ на сервере только что поменялся — смена фразы отложена, попробуйте ещё раз");
    }
    Ok(())
}

/// Каталоги на сервере — перед первой записью в них.
fn ensure_dirs(remote: &Remote, path: &str, ready: &mut (bool, bool)) -> Result<()> {
    if !ready.0 {
        remote.mkcol("")?;
        ready.0 = true;
    }
    if path.starts_with("notes/") && !ready.1 {
        remote.mkcol("notes/")?;
        ready.1 = true;
    }
    Ok(())
}

// ------------------------------------------------------------------ фоновая синхронизация

enum Msg {
    /// Синхронизироваться сейчас (после правки, по просьбе пользователя).
    Now,
    /// Новые настройки; `None` — WebDAV выключен.
    Configure(Option<Settings>),
    /// Сменить фразу шифрования на эту.
    Passphrase(String),
    /// Последняя синхронизация — и выход.
    Stop,
}

/// Чем кончилась синхронизация в фоне.
pub type Outcome = Result<Summary, String>;

/// Ошибка синхронизации для строки состояния: с «WebDAV:» впереди, но не дважды — многие
/// ошибки сервера уже начинаются с него.
pub fn message(error: &str) -> String {
    if error.starts_with("WebDAV") { error.to_string() } else { format!("WebDAV: {error}") }
}

/// Чем кончилась смена фразы: новая фраза или ошибка.
pub type Changed = Result<String, String>;

/// Синхронизация в своём потоке: сразу, раз в минуту и по просьбе; сеть не держит экран.
pub struct Worker {
    tx: Sender<Msg>,
    rx: Receiver<Outcome>,
    changed: Receiver<Changed>,
    /// Поток закончил работу (после `Stop`).
    done: Receiver<()>,
    handle: JoinHandle<()>,
}

impl Worker {
    pub fn start(dir: &Path, settings: Option<Settings>) -> Self {
        let (tx, inbox) = mpsc::channel::<Msg>();
        let (outbox, rx) = mpsc::channel::<Outcome>();
        let (changes, changed) = mpsc::channel::<Changed>();
        let (finished, done) = mpsc::channel::<()>();
        let dir = dir.to_path_buf();
        let handle = std::thread::spawn(move || {
            let book = Book::new(dir);
            let mut settings = settings;
            let run = |settings: &Option<Settings>| {
                let Some(settings) = settings else { return };
                let outcome = match sync(&book, settings) {
                    Ok(Some(summary)) => Ok(summary),
                    // Синхронизирует другой процесс.
                    Ok(None) => return,
                    Err(e) => Err(format!("{e:#}")),
                };
                let _ = outbox.send(outcome);
            };
            run(&settings);
            loop {
                match inbox.recv_timeout(EVERY) {
                    Ok(Msg::Now) | Err(RecvTimeoutError::Timeout) => run(&settings),
                    Ok(Msg::Configure(new)) => {
                        settings = new;
                        run(&settings);
                    }
                    Ok(Msg::Passphrase(new)) => {
                        let result = match &mut settings {
                            Some(s) => change_passphrase(s, &new).map(|()| s.passphrase = new.clone()),
                            None => Err(anyhow::anyhow!("WebDAV не настроен")),
                        };
                        let _ = changes.send(result.map(|()| new).map_err(|e| format!("{e:#}")));
                    }
                    Ok(Msg::Stop) => {
                        run(&settings);
                        break;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            let _ = finished.send(());
        });
        Self { tx, rx, changed, done, handle }
    }

    pub fn now(&self) {
        let _ = self.tx.send(Msg::Now);
    }

    pub fn configure(&self, settings: Option<Settings>) {
        let _ = self.tx.send(Msg::Configure(settings));
    }

    /// Сменить фразу шифрования; итог — в `passphrase_changed`. Дальше поток синхронизирует
    /// с новой фразой.
    pub fn change_passphrase(&self, new: String) {
        let _ = self.tx.send(Msg::Passphrase(new));
    }

    pub fn passphrase_changed(&self) -> Option<Changed> {
        self.changed.try_recv().ok()
    }

    /// Результаты, накопившиеся с прошлого раза.
    pub fn outcomes(&self) -> Vec<Outcome> {
        self.rx.try_iter().collect()
    }

    /// Последняя синхронизация при выходе; ждём не дольше `wait`. Не успел (нет сети) —
    /// поток не дожидаемся: процесс завершится, а файлы книжки пишутся атомарно, так что
    /// оборванная синхронизация ничего не испортит — доделается в следующий раз.
    pub fn stop(self, wait: Duration) {
        let _ = self.tx.send(Msg::Stop);
        if self.done.recv_timeout(wait).is_ok() {
            let _ = self.handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use crate::clock::Date;
    use crate::model::{Board, ListId, Task};
    use crate::notes::Notes;

    #[test]
    fn helpers() {
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"a"), "YQ==");
        // Хеш манифеста не должен меняться между версиями: эталоны FNV-1a 64.
        assert_eq!(hash(b""), "cbf29ce484222325");
        assert_eq!(hash(b"a"), "af63dc4c8601ec8c");
        assert_eq!(file_name("/dav/files/me/ztaskmgr/2026-10-26.md"), Some("2026-10-26.md".into()));
        assert_eq!(file_name("https://x/a%20b/notes/"), Some("notes".into()));
        assert!(is_book_file("2026-10-26.md") && is_book_file("notes/k3x9ab.md") && is_book_file("LISTS.md"));
        assert!(!is_book_file(".webdav-state") && !is_book_file("notes/Bad!.md") && !is_book_file("2026-10-26.md.tmp"));
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:"><d:response><d:href>/dav/book/</d:href><d:propstat><d:prop>
<d:getetag>&quot;dir&quot;</d:getetag><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
<D:response xmlns:D="DAV:"><D:href>/dav/book/2026-10-26.md</D:href><D:propstat><D:prop><lp1:getetag>"abc"</lp1:getetag>
<D:resourcetype/></D:prop></D:propstat></D:response></d:multistatus>"#;
        let entries = parse_multistatus(xml);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].collection && entries[0].etag.as_deref() == Some("\"dir\""));
        assert_eq!(
            entries[1],
            Entry { href: "/dav/book/2026-10-26.md".into(), etag: Some("\"abc\"".into()), collection: false }
        );
    }

    struct Trigger {
        method: &'static str,
        path: String,
        run: Box<dyn FnOnce(&mut Store) + Send>,
    }

    /// Простейший WebDAV-сервер в памяти: PROPFIND, GET, PUT, DELETE, MKCOL с If-Match.
    #[derive(Default)]
    struct Store {
        files: HashMap<String, (Vec<u8>, u64)>,
        dirs: BTreeSet<String>,
        version: u64,
        /// Слабые ETag (`W/"…"`): `If-Match` с ними по RFC не совпадает никогда.
        weak: bool,
        no_etag: bool,
        reject_match: bool,
        trigger: Option<Trigger>,
        requests: Vec<(String, String)>,
        unconditional: usize,
    }

    fn serve(store: Arc<Mutex<Store>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
                let mut headers = HashMap::new();
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':') {
                        headers.insert(k.trim().to_lowercase(), v.trim().to_string());
                    }
                }
                let len: usize = headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let path = path.strip_prefix("/book/").unwrap_or("").to_string();
                let mut store = store.lock().unwrap();
                store.requests.push((method.clone(), path.clone()));
                if matches!(method.as_str(), "PUT" | "DELETE")
                    && !headers.contains_key("if-match")
                    && !headers.contains_key("if-none-match")
                {
                    store.unconditional += 1;
                }
                if store.trigger.as_ref().is_some_and(|t| t.method == method && t.path == path) {
                    let trigger = store.trigger.take().unwrap();
                    (trigger.run)(&mut store);
                }
                let weak = store.weak;
                let etag_of = |v: u64| if weak { format!("W/\"v{v}\"") } else { format!("\"v{v}\"") };
                let no_etag = store.no_etag;
                let etag_header = |v| if no_etag { String::new() } else { format!("ETag: {}\r\n", etag_of(v)) };
                let current = store.files.get(&path).map(|(_, v)| etag_of(*v));
                let precondition = match (headers.get("if-match"), headers.get("if-none-match")) {
                    (Some(m), _) => current.as_ref() == Some(m) && !weak && !store.reject_match,
                    (_, Some(_)) => current.is_none(),
                    _ => true,
                };
                let (status, extra, out): (&str, String, Vec<u8>) = match method.as_str() {
                    "PROPFIND" => {
                        if !(path.is_empty() && store.dirs.contains("")) && !store.dirs.contains(&path) {
                            ("404 Not Found", String::new(), vec![])
                        } else {
                            let mut xml = String::from(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">"#);
                            xml.push_str(&format!("<d:response><d:href>/book/{path}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>"));
                            for (name, (_, v)) in &store.files {
                                if let Some(rest) = name.strip_prefix(&path)
                                    && !rest.contains('/')
                                {
                                    let tag = if no_etag {
                                        String::new()
                                    } else {
                                        format!("<d:getetag>{}</d:getetag>", etag_of(*v))
                                    };
                                    xml.push_str(&format!("<d:response><d:href>/book/{name}</d:href><d:propstat><d:prop>{tag}<d:resourcetype/></d:prop></d:propstat></d:response>"));
                                }
                            }
                            for d in &store.dirs {
                                if !d.is_empty() && path.is_empty() {
                                    xml.push_str(&format!("<d:response><d:href>/book/{d}</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>"));
                                }
                            }
                            xml.push_str("</d:multistatus>");
                            ("207 Multi-Status", String::new(), xml.into_bytes())
                        }
                    }
                    "MKCOL" => {
                        store.dirs.insert(path.clone());
                        ("201 Created", String::new(), vec![])
                    }
                    "GET" => match store.files.get(&path) {
                        Some((data, v)) => ("200 OK", etag_header(*v), data.clone()),
                        None => ("404 Not Found", String::new(), vec![]),
                    },
                    "PUT" if !precondition => ("412 Precondition Failed", String::new(), vec![]),
                    "PUT" => {
                        store.version += 1;
                        let v = store.version;
                        store.files.insert(path.clone(), (body, v));
                        ("201 Created", etag_header(v), vec![])
                    }
                    "DELETE" if !precondition => ("412 Precondition Failed", String::new(), vec![]),
                    "DELETE" => {
                        store.files.remove(&path);
                        ("204 No Content", String::new(), vec![])
                    }
                    _ => ("405 Method Not Allowed", String::new(), vec![]),
                };
                let head =
                    format!("HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n", out.len());
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&out);
            }
        });
        format!("http://{addr}/book")
    }

    fn temp_book(name: &str) -> Book {
        let dir = std::env::temp_dir().join(format!("ztaskmgr-webdav-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Book::new(dir)
    }

    fn texts(book: &Book, date: Date) -> Vec<String> {
        book.load(date).unwrap().list(ListId::WORK).iter().map(|t| t.text.clone()).collect()
    }

    #[test]
    fn two_devices() {
        let store = Arc::new(Mutex::new(Store::default()));
        let url = serve(store.clone());
        let settings = Settings {
            url,
            user: "me".into(),
            password: String::new(),
            password_command: "echo секрет".into(),
            insecure: false,
            ..Settings::default()
        };
        // Без пароля из команды не синхронизируемся; команда — один раз.
        assert!(sync(&temp_book("nopass"), &settings).is_err());
        let settings = settings.resolve().unwrap();
        assert_eq!((settings.password.as_str(), settings.needs_command()), ("секрет", false));
        let d = Date::new(2026, 10, 26).unwrap();
        let pc = temp_book("pc");
        let phone = temp_book("phone");

        // Компьютер: задача с записями — на сервер; каталоги создаются сами.
        let mut board = Board::default();
        let mut task = Task::new("отчёт");
        task.id = Some("aa1".into());
        board.push(ListId::WORK, task);
        pc.save(d, &board).unwrap();
        let mut notes = Notes::default();
        notes.add_subtask("цифры");
        pc.save_notes("aa1", &notes).unwrap();
        let s = sync(&pc, &settings).unwrap().unwrap();
        assert_eq!((s.uploaded, s.downloaded), (2, 0));
        assert!(store.lock().unwrap().files.contains_key("notes/aa1.md"));
        // Второй раз — ничего.
        assert_eq!(sync(&pc, &settings).unwrap().unwrap(), Summary::default());

        // Телефон: получает всё.
        let s = sync(&phone, &settings).unwrap().unwrap();
        assert_eq!(s.downloaded, 2);
        assert_eq!(texts(&phone, d), ["отчёт"]);

        // Оба меняют страницу — слияние, ничего не теряется.
        let mut a = pc.load(d).unwrap();
        a.push(ListId::WORK, Task::new("с компьютера"));
        pc.save(d, &a).unwrap();
        let mut b = phone.load(d).unwrap();
        b.push(ListId::WORK, Task::new("с телефона"));
        phone.save(d, &b).unwrap();
        assert_eq!(sync(&pc, &settings).unwrap().unwrap().uploaded, 1);
        assert_eq!(sync(&phone, &settings).unwrap().unwrap().merged, 1);
        assert_eq!(texts(&phone, d), ["отчёт", "с компьютера", "с телефона"]);
        assert_eq!(sync(&pc, &settings).unwrap().unwrap().downloaded, 1);
        assert_eq!(texts(&pc, d), ["отчёт", "с компьютера", "с телефона"]);

        // Удаление на одном устройстве доходит до другого.
        pc.save_notes("aa1", &Notes::default()).unwrap();
        assert_eq!(sync(&pc, &settings).unwrap().unwrap().deleted, 1);
        assert_eq!(sync(&phone, &settings).unwrap().unwrap().deleted, 1);
        assert!(!phone.dir().join("notes/aa1.md").exists());

        // Файл на сервере поменяли ещё где-то, пока мы правили свой, — слияние, а не затирание.
        let mut a = pc.load(d).unwrap();
        a.push(ListId::WORK, Task::new("ещё"));
        pc.save(d, &a).unwrap();
        {
            let mut st = store.lock().unwrap();
            st.version += 1;
            let v = st.version;
            st.files.get_mut("2026-10-26.md").unwrap().1 = v;
        }
        let s = sync(&pc, &settings).unwrap().unwrap();
        assert_eq!(s.merged, 1);

        // Блокировка: пока синхронизирует один процесс, второй пропускает.
        let lock = Lock::try_take(pc.dir()).unwrap();
        assert!(lock.is_some() && sync(&pc, &settings).unwrap().is_none());
        drop(lock);
        let _ = fs::remove_dir_all(pc.dir());
        let _ = fs::remove_dir_all(phone.dir());
    }

    #[test]
    fn weak_and_missing_etags_allow_read_and_create_but_not_destructive_changes() {
        assert_eq!(unescape("&#34;a&#x22;&amp;quot;&#zz;"), "\"a\"&quot;&#zz;");
        for (name, weak, no_etag) in [("weak", true, false), ("missing", false, true)] {
            let store = Arc::new(Mutex::new(Store { weak, no_etag, ..Store::default() }));
            let settings = Settings { url: serve(store.clone()), ..Settings::default() };
            let book = temp_book(name);
            let reader = temp_book(&format!("{name}-reader"));
            let path = "2026-10-26.md";
            let original = b"## Work <!-- list:work -->\n- [ ] original\n";
            book.write_file(path, original).unwrap();
            assert_eq!(sync(&book, &settings).unwrap().unwrap().uploaded, 1);
            assert_eq!(sync(&reader, &settings).unwrap().unwrap().downloaded, 1);
            assert_eq!(reader.read_file(path).unwrap().unwrap(), original);

            book.write_file(path, b"## Work <!-- list:work -->\n- [ ] local edit\n").unwrap();
            let summary = sync(&book, &settings).unwrap().unwrap();
            assert_eq!(summary.uploaded, 0);
            assert_eq!(summary.failed.len(), 1);
            assert!(summary.failed[0].contains("сильный ETag"), "{summary:?}");
            assert_eq!(store.lock().unwrap().files[path].0, original);

            book.remove_file(path).unwrap();
            let summary = sync(&book, &settings).unwrap().unwrap();
            assert_eq!(summary.deleted, 0);
            assert_eq!(summary.failed.len(), 1);
            assert!(summary.failed[0].contains("сильный ETag"), "{summary:?}");
            assert_eq!(store.lock().unwrap().files[path].0, original);

            // Миграция открытого текста также не заменяет его без сильного ETag.
            let secret = Settings { passphrase: "old phrase".into(), ..settings };
            let summary = sync(&reader, &secret).unwrap().unwrap();
            assert_eq!(summary.failed.len(), 1);
            assert_eq!(store.lock().unwrap().files[path].0, original);
            let key = store.lock().unwrap().files[KEY_FILE].0.clone();
            let error = change_passphrase(&secret, "new phrase").unwrap_err();
            assert!(format!("{error:#}").contains("сильный ETag"), "{error:#}");
            let st = store.lock().unwrap();
            assert_eq!(st.files[KEY_FILE].0, key);
            assert!(Cipher::unlock("old phrase", &key).is_ok());
            assert_eq!(st.unconditional, 0);
            drop(st);
            let _ = fs::remove_dir_all(book.dir());
            let _ = fs::remove_dir_all(reader.dir());
        }
    }

    #[test]
    fn concurrent_remote_put_and_delete_are_postponed_without_fallback() {
        for method in ["PUT", "DELETE"] {
            let store = Arc::new(Mutex::new(Store::default()));
            let settings = Settings { url: serve(store.clone()), ..Settings::default() };
            let book = temp_book(&format!("race-{method}"));
            let path = "2026-10-26.md";
            book.write_file(path, b"## Work <!-- list:work -->\n- [ ] original\n").unwrap();
            sync(&book, &settings).unwrap().unwrap();
            if method == "PUT" {
                book.write_file(path, b"## Work <!-- list:work -->\n- [ ] local edit\n").unwrap();
            } else {
                book.remove_file(path).unwrap();
            }
            let foreign = b"## Work <!-- list:work -->\n- [ ] OTHER DEVICE TASK\n".to_vec();
            {
                let mut st = store.lock().unwrap();
                st.requests.clear();
                let foreign = foreign.clone();
                st.trigger = Some(Trigger {
                    method,
                    path: path.into(),
                    run: Box::new(move |st| {
                        st.version += 1;
                        st.files.insert(path.into(), (foreign, st.version));
                    }),
                });
            }
            let summary = sync(&book, &settings).unwrap().unwrap();
            assert_eq!(summary.postponed, 1);
            assert_eq!((summary.uploaded, summary.deleted), (0, 0));
            let st = store.lock().unwrap();
            assert_eq!(st.files[path].0, foreign);
            assert_eq!(st.requests.iter().filter(|(m, _)| m == method).count(), 1);
            assert!(!st.requests.iter().any(|(m, _)| m == "GET"), "no compare-then-write fallback");
            assert_eq!(st.unconditional, 0);
            drop(st);
            let _ = fs::remove_dir_all(book.dir());
        }
    }

    #[test]
    fn rejected_strong_etag_is_not_retried_unconditionally() {
        let store = Arc::new(Mutex::new(Store::default()));
        let settings = Settings { url: serve(store.clone()), ..Settings::default() };
        let remote = Remote::new(&settings).unwrap();
        assert!(matches!(remote.put("2026-10-26.md", b"original", None).unwrap(), Put::Done(_)));
        let (_, etag) = remote.get("2026-10-26.md").unwrap().unwrap();
        {
            let mut st = store.lock().unwrap();
            st.reject_match = true;
            st.requests.clear();
        }
        assert_eq!(remote.replace("2026-10-26.md", b"edit", etag.as_deref()).unwrap(), Put::Changed);
        assert!(!remote.delete("2026-10-26.md", etag.as_deref().unwrap()).unwrap());
        let st = store.lock().unwrap();
        assert_eq!(st.files["2026-10-26.md"].0, b"original");
        assert_eq!(st.requests.len(), 2);
        assert_eq!(st.unconditional, 0);
    }

    #[test]
    fn rejected_passphrase_precondition_does_not_fall_back_even_if_key_is_unchanged() {
        let store = Arc::new(Mutex::new(Store { reject_match: true, ..Store::default() }));
        let settings = Settings { url: serve(store.clone()), passphrase: "old phrase".into(), ..Settings::default() };
        let (_, key) = Cipher::create(&settings.passphrase).unwrap();
        {
            let mut st = store.lock().unwrap();
            st.version = 1;
            st.files.insert(KEY_FILE.into(), (key.clone(), 1));
        }
        let error = change_passphrase(&settings, "new phrase").unwrap_err();
        assert!(format!("{error:#}").contains("смена фразы отложена"), "{error:#}");
        let st = store.lock().unwrap();
        assert_eq!(st.files[KEY_FILE].0, key);
        assert_eq!(st.requests, [("GET".to_string(), KEY_FILE.to_string()), ("PUT".to_string(), KEY_FILE.to_string())]);
        assert_eq!(st.unconditional, 0);
    }

    #[test]
    fn concurrent_passphrase_change_preserves_other_device_key() {
        let store = Arc::new(Mutex::new(Store::default()));
        let settings = Settings { url: serve(store.clone()), passphrase: "old phrase".into(), ..Settings::default() };
        let (cipher, file) = Cipher::create(&settings.passphrase).unwrap();
        let foreign = cipher.wrap("other device phrase").unwrap();
        {
            let mut st = store.lock().unwrap();
            st.version = 1;
            st.files.insert(KEY_FILE.into(), (file, 1));
            let foreign = foreign.clone();
            st.trigger = Some(Trigger {
                method: "PUT",
                path: KEY_FILE.into(),
                run: Box::new(move |st| {
                    st.version += 1;
                    st.files.insert(KEY_FILE.into(), (foreign, st.version));
                }),
            });
        }
        let error = change_passphrase(&settings, "my new phrase").unwrap_err();
        assert!(format!("{error:#}").contains("смена фразы отложена"), "{error:#}");
        let st = store.lock().unwrap();
        assert_eq!(st.files[KEY_FILE].0, foreign);
        assert!(Cipher::unlock("other device phrase", &st.files[KEY_FILE].0).is_ok());
        assert_eq!(st.requests.iter().filter(|(m, _)| m == "PUT").count(), 1);
        assert_eq!(st.requests.iter().filter(|(m, _)| m == "GET").count(), 1);
        assert_eq!(st.unconditional, 0);
    }

    #[test]
    fn download_and_merge_recheck_local_commit_after_http_without_holding_book_lock() {
        for (name, unreadable, merge) in [
            ("download-local-race", false, false),
            ("download-read-error", true, false),
            ("merge-local-race", false, true),
            ("merge-read-error", true, true),
        ] {
            let store = Arc::new(Mutex::new(Store::default()));
            let settings = Settings { url: serve(store.clone()), ..Settings::default() };
            let book = temp_book(name);
            let path = "2026-10-26.md";
            let original = b"## Work <!-- list:work -->\n- [ ] original\n";
            let foreign = b"## Work <!-- list:work -->\n- [ ] local writer\n";
            book.write_file(path, original).unwrap();
            sync(&book, &settings).unwrap().unwrap();
            if merge {
                book.write_file(path, b"## Work <!-- list:work -->\n- [ ] my edit\n").unwrap();
            }
            let dir = book.dir().to_path_buf();
            {
                let mut st = store.lock().unwrap();
                st.version += 1;
                let version = st.version;
                st.files.insert(path.into(), (b"## Work <!-- list:work -->\n- [ ] remote edit\n".to_vec(), version));
                st.trigger = Some(Trigger {
                    method: "GET",
                    path: path.into(),
                    run: Box::new(move |_| {
                        // Этот писатель работает в HTTP-потоке до ответа: локальная
                        // блокировка sync здесь не удерживается.
                        let writer = Book::new(dir);
                        let _guard = writer.lock().unwrap();
                        if unreadable {
                            fs::remove_file(writer.dir().join(path)).unwrap();
                            fs::create_dir(writer.dir().join(path)).unwrap();
                        } else {
                            writer.write_file(path, foreign).unwrap();
                        }
                    }),
                });
            }
            let summary = sync(&book, &settings).unwrap().unwrap();
            assert_eq!(summary.downloaded, 0);
            if unreadable {
                assert_eq!(summary.failed.len(), 1);
                assert!(book.dir().join(path).is_dir());
            } else {
                assert_eq!(summary.postponed, 1);
                assert_eq!(book.read_file(path).unwrap().unwrap(), foreign);
            }
            let _ = fs::remove_dir_all(book.dir());
        }
    }

    #[test]
    fn unreadable_merge_result_does_not_delete_remote_or_advance_manifest() {
        use crate::store::{FsStore, Stamp, Store as LocalStore};
        use std::cell::Cell;
        use std::rc::Rc;

        struct FailAfterWrite {
            inner: FsStore,
            armed: Rc<Cell<bool>>,
            failed: Cell<bool>,
        }

        impl LocalStore for FailAfterWrite {
            fn read(&self, path: &str) -> Result<Option<Vec<u8>>> {
                if path == "2026-10-26.md" && self.failed.get() {
                    bail!("injected read failure after merge");
                }
                self.inner.read(path)
            }

            fn write(&self, path: &str, data: &[u8]) -> Result<()> {
                self.inner.write(path, data)?;
                if path == "2026-10-26.md" && self.armed.get() {
                    self.failed.set(true);
                }
                Ok(())
            }

            fn remove(&self, path: &str) -> Result<()> {
                self.inner.remove(path)
            }
            fn list(&self, dir: &str) -> Result<Vec<String>> {
                self.inner.list(dir)
            }
            fn stamp(&self, path: &str) -> Option<Stamp> {
                self.inner.stamp(path)
            }
            fn lock(&self) -> Result<Option<fs::File>> {
                self.inner.lock()
            }
        }

        let store = Arc::new(Mutex::new(Store::default()));
        let settings = Settings { url: serve(store.clone()), ..Settings::default() };
        let dir = temp_book("merge-result-error").dir().to_path_buf();
        let armed = Rc::new(Cell::new(false));
        let book = Book::with_store(
            dir.clone(),
            Box::new(FailAfterWrite { inner: FsStore::new(dir), armed: armed.clone(), failed: Cell::new(false) }),
        );
        let path = "2026-10-26.md";
        book.write_file(path, b"## Work <!-- list:work -->\n- [ ] original\n").unwrap();
        sync(&book, &settings).unwrap().unwrap();
        let manifest = fs::read(book.dir().join(STATE_FILE)).unwrap();
        book.write_file(path, b"## Work <!-- list:work -->\n- [ ] local edit\n").unwrap();
        let foreign = b"## Work <!-- list:work -->\n- [ ] remote edit\n";
        {
            let mut st = store.lock().unwrap();
            st.version += 1;
            let version = st.version;
            st.files.insert(path.into(), (foreign.to_vec(), version));
            st.requests.clear();
        }
        armed.set(true);
        let summary = sync(&book, &settings).unwrap().unwrap();
        assert_eq!(summary.failed.len(), 1);
        assert!(summary.failed[0].contains("injected read failure"), "{summary:?}");
        assert_eq!((summary.uploaded, summary.merged, summary.deleted), (0, 0, 0));
        assert_eq!(fs::read(book.dir().join(STATE_FILE)).unwrap(), manifest);
        let merged = fs::read_to_string(book.dir().join(path)).unwrap();
        assert!(merged.contains("local edit") && merged.contains("remote edit"));
        let st = store.lock().unwrap();
        assert_eq!(st.files[path].0, foreign);
        assert!(!st.requests.iter().any(|(m, _)| matches!(m.as_str(), "PUT" | "DELETE")));
        drop(st);
        let _ = fs::remove_dir_all(book.dir());
    }

    #[test]
    fn http_password_exception_requires_numeric_loopback_or_exact_localhost() {
        for host in ["127.0.0.1:8080", "127.99.10.1", "[::1]:8080", "[0:0:0:0:0:0:0:1]", "localhost", "LOCALHOST"] {
            let settings = Settings {
                url: format!("http://{host}/book"),
                user: "me".into(),
                ca: "/nonexistent/ca.pem".into(),
                ..Settings::default()
            };
            assert!(is_loopback(&settings.url), "{host}");
            let error = Remote::new(&settings).err().expect("missing CA file");
            assert!(format!("{error:#}").contains("webdav_ca"), "{host}: {error:#}");
        }
        for host in [
            "127.evil",
            "127.outside.example.invalid",
            "127.0.0.1.evil",
            "127.999.1.1",
            "127.1",
            "localhost.evil",
            "localhost.",
            "192.168.1.5",
            "[::2]",
        ] {
            let settings = Settings {
                url: format!("http://{host}/book"),
                user: "me".into(),
                ca: "/nonexistent/ca.pem".into(),
                ..Settings::default()
            };
            assert!(!is_loopback(&settings.url), "{host}");
            let error = Remote::new(&settings).err().expect("external HTTP password rejected");
            let error = format!("{error:#}");
            assert!(error.contains("https://") && !error.contains("webdav_ca"), "{host}: {error}");
        }
    }

    #[test]
    fn bad_file_does_not_block_others() {
        let store = Arc::new(Mutex::new(Store::default()));
        let url = serve(store.clone());
        {
            let mut st = store.lock().unwrap();
            st.dirs.insert(String::new());
            st.files.insert("2026-10-25.md".into(), (vec![0xff, 0xfe], 1));
            st.files.insert("2026-10-26.md".into(), (b"## Work <!-- list:work -->\n- [ ] good\n".to_vec(), 2));
            st.version = 2;
        }
        let settings = Settings {
            url,
            user: String::new(),
            password: String::new(),
            password_command: String::new(),
            insecure: false,
            ..Settings::default()
        };
        let book = temp_book("bad");
        let s = sync(&book, &settings).unwrap().unwrap();
        assert_eq!(s.downloaded, 1);
        assert_eq!(s.failed.len(), 1);
        assert!(s.failed[0].starts_with("2026-10-25.md: не текст"), "{:?}", s.failed);
        assert_eq!(texts(&book, Date::new(2026, 10, 26).unwrap()), ["good"]);
        // Хорошее запомнено: в следующий раз не скачивается заново, плохое — снова в `failed`.
        let s = sync(&book, &settings).unwrap().unwrap();
        assert_eq!((s.downloaded, s.failed.len()), (0, 1));
        let _ = fs::remove_dir_all(book.dir());
    }

    #[test]
    fn worker() {
        let store = Arc::new(Mutex::new(Store::default()));
        let url = serve(store.clone());
        let settings = Settings {
            url,
            user: String::new(),
            password: String::new(),
            password_command: String::new(),
            insecure: false,
            ..Settings::default()
        };
        let book = temp_book("worker");
        let mut board = Board::default();
        board.push(ListId::WORK, Task::new("из потока"));
        book.save(Date::new(2026, 10, 26).unwrap(), &board).unwrap();
        // Сразу при запуске — синхронизация, итог приходит в канал.
        let worker = Worker::start(book.dir(), Some(settings));
        let first = (0..100)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(20));
                worker.outcomes().pop()
            })
            .expect("итог синхронизации");
        assert_eq!(first.unwrap().uploaded, 1);
        assert!(store.lock().unwrap().files.contains_key("2026-10-26.md"));
        // Без настроек — ничего не делает; остановка не зависает.
        worker.configure(None);
        let started = std::time::Instant::now();
        worker.stop(Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = fs::remove_dir_all(book.dir());
    }

    #[test]
    fn encrypted() {
        let store = Arc::new(Mutex::new(Store::default()));
        let url = serve(store.clone());
        let plain = Settings { url, ..Settings::default() };
        let d = Date::new(2026, 10, 26).unwrap();
        let pc = temp_book("enc-pc");
        let phone = temp_book("enc-phone");
        let on_server = |path: &str| store.lock().unwrap().files.get(path).map(|(data, _)| data.clone());

        // Книжка без шифрования уже на сервере.
        let mut board = Board::default();
        let mut task = Task::new("секретный отчёт");
        task.id = Some("aa1".into());
        board.push(ListId::WORK, task);
        pc.save(d, &board).unwrap();
        let mut notes = Notes::default();
        notes.add_subtask("пин-код");
        pc.save_notes("aa1", &notes).unwrap();
        assert_eq!(sync(&pc, &plain).unwrap().unwrap().uploaded, 2);

        // Включили шифрование: ключ на сервере, все файлы перезаписаны зашифрованными.
        let secret = Settings { passphrase: "фраза".into(), ..plain.clone() };
        sync(&pc, &secret).unwrap().unwrap();
        assert!(on_server(KEY_FILE).is_some());
        for path in ["2026-10-26.md", "notes/aa1.md"] {
            let data = on_server(path).unwrap();
            assert!(crypt::is_sealed(&data), "{path}");
            assert!(
                !String::from_utf8_lossy(&data).contains("секрет") && !String::from_utf8_lossy(&data).contains("пин")
            );
        }
        // Дальше — ничего лишнего.
        assert_eq!(sync(&pc, &secret).unwrap().unwrap(), Summary::default());

        // Телефон без фразы и с неверной — ошибка, и в книжке ничего не меняется.
        let err = sync(&phone, &plain).unwrap_err();
        assert!(format!("{err:#}").contains("зашифрована"), "{err:#}");
        let wrong = Settings { passphrase: "не та".into(), ..plain.clone() };
        let err = sync(&phone, &wrong).unwrap_err();
        assert!(format!("{err:#}").contains("неверная фраза"), "{err:#}");
        assert!(phone.files().unwrap().is_empty());

        // С верной — получает всё открытым текстом.
        assert_eq!(sync(&phone, &secret).unwrap().unwrap().downloaded, 2);
        assert_eq!(texts(&phone, d), ["секретный отчёт"]);
        assert_eq!(phone.load_notes("aa1").unwrap(), notes);

        // Правки с обеих сторон сливаются.
        let mut a = pc.load(d).unwrap();
        a.push(ListId::WORK, Task::new("с компьютера"));
        pc.save(d, &a).unwrap();
        let mut b = phone.load(d).unwrap();
        b.push(ListId::WORK, Task::new("с телефона"));
        phone.save(d, &b).unwrap();
        assert_eq!(sync(&pc, &secret).unwrap().unwrap().uploaded, 1);
        assert_eq!(sync(&phone, &secret).unwrap().unwrap().merged, 1);
        assert_eq!(sync(&pc, &secret).unwrap().unwrap().downloaded, 1);
        assert_eq!(texts(&pc, d), ["секретный отчёт", "с компьютера", "с телефона"]);
        assert!(crypt::is_sealed(&on_server("2026-10-26.md").unwrap()));

        // Открытый файл с устройства без шифрования (старой версии) принимается и шифруется.
        {
            let mut st = store.lock().unwrap();
            st.version += 1;
            let v = st.version;
            st.files
                .insert("2026-10-27.md".into(), ("## Work <!-- list:work -->\n- [ ] открыто\n".as_bytes().to_vec(), v));
        }
        assert_eq!(sync(&pc, &secret).unwrap().unwrap().downloaded, 1);
        assert_eq!(texts(&pc, Date::new(2026, 10, 27).unwrap()), ["открыто"]);
        assert!(crypt::is_sealed(&on_server("2026-10-27.md").unwrap()));

        // Сервер подменил одну страницу другой — не расшифровывается, книжка не тронута.
        {
            let mut st = store.lock().unwrap();
            let page = st.files["2026-10-26.md"].0.clone();
            st.version += 1;
            let v = st.version;
            st.files.insert("2026-10-27.md".into(), (page, v));
        }
        let s = sync(&pc, &secret).unwrap().unwrap();
        assert_eq!(s.failed.len(), 1);
        assert!(s.failed[0].starts_with("2026-10-27.md: не расшифровывается"), "{:?}", s.failed);
        assert_eq!(texts(&pc, Date::new(2026, 10, 27).unwrap()), ["открыто"]);

        // Смена фразы: файлы на сервере те же, старая фраза больше не подходит.
        let before: Vec<_> = ["2026-10-26.md", "notes/aa1.md"].map(on_server).into();
        assert!(change_passphrase(&plain, "новая").is_err());
        assert!(change_passphrase(&wrong, "новая").is_err());
        change_passphrase(&secret, "новая").unwrap();
        assert_eq!(before, ["2026-10-26.md", "notes/aa1.md"].map(on_server));
        let err = sync(&phone, &secret).unwrap_err();
        assert!(format!("{err:#}").contains("неверная фраза"), "{err:#}");
        let renewed = Settings { passphrase: "новая".into(), ..plain.clone() };
        // Не расшифровывается только подменённая выше страница.
        let s = sync(&phone, &renewed).unwrap().unwrap();
        assert!(s.failed.len() == 1 && s.failed[0].starts_with("2026-10-27.md"), "{:?}", s.failed);
        assert_eq!(texts(&phone, d), ["секретный отчёт", "с компьютера", "с телефона"]);

        // Фраза из команды — как пароль.
        let command = Settings { passphrase_command: "echo фраза".into(), ..plain.clone() };
        assert!(command.needs_command());
        assert_eq!(command.resolve().unwrap().passphrase, "фраза");
        assert!(Settings { passphrase_command: "true".into(), ..plain }.resolve().is_err());
        let _ = fs::remove_dir_all(pc.dir());
        let _ = fs::remove_dir_all(phone.dir());
    }

    /// HTTPS-сервер с сертификатом, подписанным своим (тестовым) центром; на всё отвечает 404 —
    /// книжки на сервере ещё нет.
    fn serve_tls() -> String {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let certs = CertificateDer::pem_slice_iter(include_bytes!("../testdata/test-server.crt"))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(include_bytes!("../testdata/test-server.key")).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let mut tls = rustls::StreamOwned::new(rustls::ServerConnection::new(config.clone()).unwrap(), stream);
                // Заголовки — до пустой строки; короткое тело PROPFIND приходит с ними.
                let (mut buf, mut got) = ([0u8; 4096], Vec::new());
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = tls.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                tls.conn.send_close_notify();
                let _ = tls.flush();
            }
        });
        format!("https://{addr}/book/")
    }

    #[test]
    fn own_ca() {
        let ca = include_bytes!("../testdata/test-ca.crt");
        assert_eq!(parse_ca(ca).unwrap().len(), 1);
        assert!(parse_ca(b"garbage").is_err());
        assert!(parse_ca(include_bytes!("../testdata/test-server.key")).is_err());

        let book = temp_book("ca");
        let plain = Settings { url: serve_tls(), ..Settings::default() };
        // Свой центр неизвестен — соединение не проходит проверку.
        let err = format!("{:#}", sync(&book, &plain).unwrap_err());
        assert!(err.contains("UnknownIssuer"), "{err}");
        // С ним — проходит.
        let path = book.dir().with_extension("ca.pem");
        fs::write(&path, ca).unwrap();
        let trusted = Settings { ca: path.to_string_lossy().into(), ..plain.clone() };
        assert_eq!(sync(&book, &trusted).unwrap().unwrap(), Summary::default());
        // Нет файла — понятная ошибка, а не «не тот издатель».
        let missing = Settings { ca: "/nonexistent/ca.pem".into(), ..plain };
        let err = format!("{:#}", sync(&book, &missing).unwrap_err());
        assert!(err.contains("webdav_ca"), "{err}");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(book.dir());
    }

    #[test]
    fn errors() {
        let book = temp_book("err");
        let bad = Settings {
            url: "ftp://x".into(),
            user: String::new(),
            password: String::new(),
            password_command: String::new(),
            insecure: false,
            ..Settings::default()
        };
        // Пароль по http:// в сеть — нельзя, на этот же компьютер — можно.
        let open = Settings { url: "http://dav.example.com/book".into(), user: "me".into(), ..bad.clone() };
        let err = sync(&book, &open).unwrap_err();
        assert!(format!("{err:#}").contains("https://"), "{err:#}");
        assert!(
            is_loopback("http://127.0.0.1:8080/x")
                && is_loopback("http://me@localhost/x")
                && is_loopback("http://[::1]:80/")
        );
        assert!(!is_loopback("http://192.168.1.5/x") && !is_loopback("http://localhost.evil.com/"));
        assert!(sync(&book, &bad).is_err());
        // Сервер не отвечает — ошибка, а не зависание.
        let gone = Settings { url: "http://127.0.0.1:9/book".into(), ..bad };
        assert!(sync(&book, &gone).is_err());
        let _ = fs::remove_dir_all(book.dir());
    }
}
