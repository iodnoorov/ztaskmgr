//! Синхронизация книжки с WebDAV-сервером пользователя (Nextcloud, ownCloud, Apache, rclone…).
//!
//! Пофайлово: страницы, записи задач (`notes/`), `LISTS.md`, `DELETED_NOTES.md`. Что было
//! на сервере и у нас при прошлой синхронизации, помнит манифест `.webdav-state` (ETag сервера
//! и хеш содержимого по каждому файлу). По нему видно, кто что менял:
//!
//! - изменилось только у нас — PUT (с `If-Match`, чтобы не затереть чужую запись), удалили
//!   у нас — DELETE;
//! - только на сервере — GET, удалили там — удаляем у себя;
//! - с обеих сторон — слияние, как у копий Syncthing (`sync.rs`), и PUT результата.
//!
//! Сервер ответил «412: файл уже другой» — файл пропускается, в следующий раз он сольётся.
//! Несколько процессов (программа и MCP-сервер) не синхронизируются одновременно —
//! `.webdav-lock`.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::book::Book;
use crate::clock::Date;
use crate::lists::LISTS_FILE;
use crate::book::DELETED_NOTES;
use crate::storage;

const STATE_FILE: &str = ".webdav-state";
const LOCK_FILE: &str = ".webdav-lock";

/// Сколько ждать сервер, прежде чем сдаться.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Как часто синхронизироваться, пока программа открыта.
pub const EVERY: Duration = Duration::from_secs(60);

/// Настройки подключения (из `config`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Каталог книжки на сервере, например `https://cloud.example.com/remote.php/dav/files/me/ztaskmgr/`.
    pub url: String,
    pub user: String,
    pub password: String,
    /// Команда, которая печатает пароль (`pass ztaskmgr`); если задана — главнее `password`.
    pub password_command: String,
}

impl Settings {
    fn password(&self) -> Result<String> {
        if self.password_command.trim().is_empty() {
            return Ok(self.password.clone());
        }
        let out = Command::new("sh")
            .arg("-c")
            .arg(&self.password_command)
            .output()
            .with_context(|| format!("запуск «{}»", self.password_command))?;
        if !out.status.success() {
            bail!("команда пароля «{}» завершилась с ошибкой", self.password_command);
        }
        let text = String::from_utf8(out.stdout).context("команда пароля вывела не текст")?;
        Ok(text.lines().next().unwrap_or("").to_string())
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
        let auth = (!settings.user.is_empty())
            .then(|| -> Result<String> {
                let password = settings.password()?;
                Ok(format!("Basic {}", base64(format!("{}:{password}", settings.user).as_bytes())))
            })
            .transpose()?;
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .allow_non_standard_methods(true)
            .timeout_global(Some(TIMEOUT))
            .build()
            .into();
        Ok(Self { agent, base, auth })
    }

    /// Запрос к файлу или каталогу `path` (относительно `base`): код ответа, ETag и тело.
    fn call(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<(u16, Option<String>, Vec<u8>)> {
        let mut request = ureq::http::Request::builder().method(method).uri(format!("{}{path}", self.base));
        if let Some(auth) = &self.auth {
            request = request.header("Authorization", auth);
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request.body(body.to_vec()).context("запрос WebDAV")?;
        let mut response = self
            .agent
            .run(request)
            .with_context(|| format!("WebDAV: {method} {}{path}", self.base))?;
        let status = response.status().as_u16();
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
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

    /// Содержимое и ETag; файла нет — `None`.
    fn get(&self, path: &str) -> Result<Option<(Vec<u8>, Option<String>)>> {
        let (status, etag, body) = self.call("GET", path, &[], b"")?;
        match status {
            200 => Ok(Some((body, etag))),
            404 => Ok(None),
            _ => Self::fail("GET", path, status),
        }
    }

    /// Запись, если на сервере всё ещё `expected` (`None` — файла там быть не должно).
    fn put(&self, path: &str, data: &[u8], expected: Option<&str>) -> Result<Put> {
        let condition = match expected {
            Some(etag) => ("If-Match", etag),
            None => ("If-None-Match", "*"),
        };
        let headers = [condition, ("Content-Type", "text/markdown; charset=utf-8")];
        let (status, etag, _) = self.call("PUT", path, &headers, data)?;
        match status {
            200..=299 => Ok(Put::Done(etag)),
            412 => Ok(Put::Changed),
            _ => Self::fail("PUT", path, status),
        }
    }

    /// Удаление, если на сервере всё ещё `etag`. `false` — файл там уже другой.
    fn delete(&self, path: &str, etag: &str) -> Result<bool> {
        let (status, _, _) = self.call("DELETE", path, &[("If-Match", etag)], b"")?;
        match status {
            200..=299 | 404 => Ok(true),
            412 => Ok(false),
            _ => Self::fail("DELETE", path, status),
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

fn unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
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
    match path.strip_prefix("notes/") {
        Some(name) => name.strip_suffix(".md").is_some_and(storage::is_valid_id),
        None => {
            path == LISTS_FILE
                || path == DELETED_NOTES
                || path.strip_suffix(".md").and_then(Date::parse_iso).is_some()
        }
    }
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

/// Манифест: для какого адреса и что было по каждому файлу. Другой адрес — всё сначала.
fn load_state(dir: &Path, url: &str) -> HashMap<String, Known> {
    let Ok(text) = fs::read_to_string(dir.join(STATE_FILE)) else {
        return HashMap::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return HashMap::new();
    };
    if value["url"].as_str() != Some(url) {
        return HashMap::new();
    }
    value["files"]
        .as_object()
        .map(|files| {
            files
                .iter()
                .filter_map(|(path, v)| {
                    let known = Known { etag: v["etag"].as_str().map(str::to_string), hash: v["hash"].as_str()?.to_string() };
                    Some((path.clone(), known))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_state(dir: &Path, url: &str, state: &HashMap<String, Known>) -> Result<()> {
    let files: serde_json::Map<String, Value> = state
        .iter()
        .map(|(path, k)| (path.clone(), json!({"etag": k.etag, "hash": k.hash})))
        .collect();
    let text = serde_json::to_string_pretty(&json!({"url": url, "files": files}))?;
    storage::write_atomic(&dir.join(STATE_FILE), &text)
}

/// Файлы книжки у нас: путь → содержимое.
fn local_files(dir: &Path) -> Result<HashMap<String, Vec<u8>>> {
    let mut files = HashMap::new();
    for (sub, prefix) in [(dir.to_path_buf(), ""), (dir.join("notes"), "notes/")] {
        let entries = match fs::read_dir(&sub) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("чтение {}", sub.display())),
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let Ok(name) = entry.file_name().into_string() else { continue };
            let path = format!("{prefix}{name}");
            if is_book_file(&path) && entry.path().is_file() {
                files.insert(path, fs::read(entry.path()).with_context(|| format!("чтение {name}"))?);
            }
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
    let remote = Remote::new(settings)?;
    let mut state = load_state(dir, &remote.base);
    let mut summary = Summary::default();

    // Что на сервере. Нет каталога книжки — создадим при первой записи.
    let top = remote.list("")?;
    let notes = if top.is_some() { remote.list("notes/")? } else { None };
    let mut dirs_ready = (top.is_some(), notes.is_some());
    let mut server: HashMap<String, Option<String>> = HashMap::new();
    for (prefix, files) in [("", top), ("notes/", notes)] {
        for (name, etag) in files.unwrap_or_default() {
            let path = format!("{prefix}{name}");
            if is_book_file(&path) {
                server.insert(path, etag);
            }
        }
    }
    let local = local_files(dir)?;
    let paths: BTreeSet<&String> = local.keys().chain(server.keys()).chain(state.keys()).collect();
    let paths: Vec<String> = paths.into_iter().cloned().collect();

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
        let file = dir.join(&path);
        // Пишем в файл, только если он всё ещё такой, каким мы его прочитали: программа
        // могла сохранить его, пока мы ходили на сервер.
        let unchanged_here = || fs::read(&file).ok().map(|d| hash(&d)) == my_hash;

        match (local_changed, remote_changed, mine, theirs) {
            (false, false, ..) => {}
            // Только у нас.
            (true, false, Some(data), _) => {
                ensure_dirs(&remote, &path, &mut dirs_ready)?;
                let expected = theirs.and_then(|_| known.as_ref().and_then(|k| k.etag.as_deref()));
                match remote.put(&path, data, expected)? {
                    Put::Done(etag) => {
                        state.insert(path, Known { etag, hash: my_hash.unwrap() });
                        summary.uploaded += 1;
                    }
                    Put::Changed => summary.postponed += 1,
                }
            }
            (true, false, None, Some(_)) => {
                let etag = known.as_ref().and_then(|k| k.etag.clone()).unwrap_or_default();
                if remote.delete(&path, &etag)? {
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
                    let new_hash = hash(&data);
                    if my_hash.as_ref() != Some(&new_hash) {
                        if !unchanged_here() {
                            summary.postponed += 1;
                            continue;
                        }
                        let text = String::from_utf8(data).with_context(|| format!("{path}: не текст"))?;
                        storage::write_atomic(&file, &text)?;
                        summary.downloaded += 1;
                    }
                    state.insert(path, Known { etag, hash: new_hash });
                }
                None => summary.postponed += 1,
            },
            (false, true, Some(_), None) => {
                if unchanged_here() {
                    fs::remove_file(&file).with_context(|| format!("удаление {path}"))?;
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
                let their_hash = hash(&data);
                if my_hash.as_ref() == Some(&their_hash) {
                    state.insert(path, Known { etag, hash: their_hash });
                    continue;
                }
                if !unchanged_here() {
                    summary.postponed += 1;
                    continue;
                }
                let text = String::from_utf8(data).with_context(|| format!("{path}: не текст"))?;
                let (notes, base) = match path.strip_prefix("notes/") {
                    Some(name) => (true, name),
                    None => (false, path.as_str()),
                };
                book.merge_text(base, notes, &text).with_context(|| format!("слияние {path}"))?;
                let merged = fs::read(&file).ok();
                let result = match (&merged, etag.as_deref()) {
                    (Some(merged), _) => remote.put(&path, merged, etag.as_deref())?,
                    // Слияние дало пустую страницу — файла нет и на сервере.
                    (None, Some(etag)) => {
                        if remote.delete(&path, etag)? { Put::Done(None) } else { Put::Changed }
                    }
                    (None, None) => Put::Changed,
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
    save_state(dir, &remote.base, &state)?;
    Ok(Some(summary))
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
    /// Последняя синхронизация — и выход.
    Stop,
}

/// Чем кончилась синхронизация в фоне.
pub type Outcome = Result<Summary, String>;

/// Синхронизация в своём потоке: сразу, раз в минуту и по просьбе; сеть не держит экран.
pub struct Worker {
    tx: Sender<Msg>,
    rx: Receiver<Outcome>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn start(dir: &Path, settings: Option<Settings>) -> Self {
        let (tx, inbox) = mpsc::channel::<Msg>();
        let (outbox, rx) = mpsc::channel::<Outcome>();
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
                    Ok(Msg::Stop) => {
                        run(&settings);
                        break;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self { tx, rx, handle: Some(handle) }
    }

    pub fn now(&self) {
        let _ = self.tx.send(Msg::Now);
    }

    pub fn configure(&self, settings: Option<Settings>) {
        let _ = self.tx.send(Msg::Configure(settings));
    }

    /// Результаты, накопившиеся с прошлого раза.
    pub fn outcomes(&self) -> Vec<Outcome> {
        self.rx.try_iter().collect()
    }

    /// Последняя синхронизация при выходе; ждём не дольше `wait`.
    pub fn stop(mut self, wait: Duration) {
        let _ = self.tx.send(Msg::Stop);
        let deadline = std::time::Instant::now() + wait;
        if let Some(handle) = self.handle.take() {
            while !handle.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use crate::model::{Board, ListId, Task};
    use crate::notes::Notes;

    #[test]
    fn helpers() {
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"a"), "YQ==");
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
        assert_eq!(entries[1], Entry { href: "/dav/book/2026-10-26.md".into(), etag: Some("\"abc\"".into()), collection: false });
    }

    /// Простейший WebDAV-сервер в памяти: PROPFIND, GET, PUT, DELETE, MKCOL с If-Match.
    #[derive(Default)]
    struct Store {
        files: HashMap<String, (Vec<u8>, u64)>,
        dirs: BTreeSet<String>,
        version: u64,
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
                let etag_of = |v: u64| format!("\"v{v}\"");
                let current = store.files.get(&path).map(|(_, v)| etag_of(*v));
                let precondition = match (headers.get("if-match"), headers.get("if-none-match")) {
                    (Some(m), _) => current.as_ref() == Some(m),
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
                                if let Some(rest) = name.strip_prefix(&path) && !rest.contains('/') {
                                    xml.push_str(&format!("<d:response><d:href>/book/{name}</d:href><d:propstat><d:prop><d:getetag>{}</d:getetag><d:resourcetype/></d:prop></d:propstat></d:response>", etag_of(*v)));
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
                        Some((data, v)) => ("200 OK", format!("ETag: {}\r\n", etag_of(*v)), data.clone()),
                        None => ("404 Not Found", String::new(), vec![]),
                    },
                    "PUT" if !precondition => ("412 Precondition Failed", String::new(), vec![]),
                    "PUT" => {
                        store.version += 1;
                        let v = store.version;
                        store.files.insert(path.clone(), (body, v));
                        ("201 Created", format!("ETag: {}\r\n", etag_of(v)), vec![])
                    }
                    "DELETE" if !precondition => ("412 Precondition Failed", String::new(), vec![]),
                    "DELETE" => {
                        store.files.remove(&path);
                        ("204 No Content", String::new(), vec![])
                    }
                    _ => ("405 Method Not Allowed", String::new(), vec![]),
                };
                let head = format!("HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n", out.len());
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
        let settings = Settings { url, user: "me".into(), password: String::new(), password_command: "echo секрет".into() };
        assert_eq!(settings.password().unwrap(), "секрет");
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
    fn errors() {
        let book = temp_book("err");
        let bad = Settings { url: "ftp://x".into(), user: String::new(), password: String::new(), password_command: String::new() };
        assert!(sync(&book, &bad).is_err());
        // Сервер не отвечает — ошибка, а не зависание.
        let gone = Settings { url: "http://127.0.0.1:9/book".into(), ..bad };
        assert!(sync(&book, &gone).is_err());
        let _ = fs::remove_dir_all(book.dir());
    }
}
