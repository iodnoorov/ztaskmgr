//! Шифрование книжки на сервере WebDAV. На устройствах книжка — обычный Markdown, а на сервер
//! файлы уходят зашифрованными: хостинг видит имена файлов (даты страниц, id записей), размеры
//! и время правки, но не содержимое.
//!
//! - Файл на сервере: `MAGIC`, nonce (24 байта) и шифротекст XChaCha20-Poly1305. Связанные
//!   данные — путь файла: подменить одну страницу другой сервер не может.
//! - Файлы шифруются ключом книжки — 32 случайных байта. Он лежит на сервере в `KEY_FILE`
//!   зашифрованным ключом из парольной фразы (Argon2id с солью оттуда же), поэтому все
//!   устройства с одной фразой получают один ключ, а неверная фраза видна сразу — ключ
//!   не расшифровывается. Сменить фразу — перешифровать только `KEY_FILE`.

use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde_json::{Value, json};

/// Ключ книжки на сервере.
pub const KEY_FILE: &str = ".ztaskmgr-key";

/// Начало зашифрованного файла.
const MAGIC: &[u8] = b"ztaskmgr encrypted 1\n";

const NONCE: usize = 24;

/// Связанные данные ключа книжки в `KEY_FILE`.
const KEY_AAD: &[u8] = b"ztaskmgr book key";

/// Argon2id: память (КиБ), проходы, потоки. Хранятся в `KEY_FILE`, так что их можно менять,
/// не ломая старые книжки. В тестах — дешёвые: отладочная сборка считает Argon2 медленно.
const PARAMS: (u32, u32, u32) = if cfg!(test) { (64, 1, 1) } else { (64 * 1024, 3, 1) };

/// Больше не считаем, что бы ни было в `KEY_FILE`: файл с сервера, и он не должен съесть
/// всю память телефона.
const MAX_MEMORY_KIB: u32 = 1024 * 1024;
const MAX_PASSES: u32 = 16;

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    OsRng.try_fill_bytes(&mut bytes).map_err(|e| anyhow!("нет случайных чисел: {e}"))?;
    Ok(bytes)
}

/// Ключ из фразы. Argon2 дорог (доли секунды и десятки мегабайт), а синхронизация — раз
/// в минуту, поэтому последний результат помнится до конца работы процесса.
fn derive(passphrase: &str, salt: &[u8], (memory, passes, lanes): (u32, u32, u32)) -> Result<[u8; 32]> {
    type Cached = (String, Vec<u8>, (u32, u32, u32), [u8; 32]);
    static CACHE: Mutex<Option<Cached>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((p, s, params, key)) = cache.as_ref()
        && p == passphrase
        && s == salt
        && *params == (memory, passes, lanes)
    {
        return Ok(*key);
    }
    if memory > MAX_MEMORY_KIB || passes > MAX_PASSES || lanes > 16 {
        bail!("ключ книжки на сервере требует слишком много ресурсов");
    }
    let params = Params::new(memory, passes, lanes, Some(32)).map_err(|e| anyhow!("Argon2: {e}"))?;
    let mut key = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow!("Argon2: {e}"))?;
    *cache = Some((passphrase.to_string(), salt.to_vec(), (memory, passes, lanes), key));
    Ok(key)
}

fn seal(key: &[u8; 32], aad: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let sealed = XChaCha20Poly1305::new(key.into())
        .encrypt(&nonce, Payload { msg: data, aad })
        .map_err(|_| anyhow!("шифрование не удалось"))?;
    Ok([nonce.as_slice(), &sealed].concat())
}

/// `None` — не расшифровывается: другой ключ, другой путь или файл испорчен.
fn open(key: &[u8; 32], aad: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < NONCE {
        return None;
    }
    let (nonce, sealed) = data.split_at(NONCE);
    XChaCha20Poly1305::new(key.into()).decrypt(XNonce::from_slice(nonce), Payload { msg: sealed, aad }).ok()
}

/// Ключ книжки.
#[derive(Clone)]
pub struct Cipher {
    key: [u8; 32],
}

/// Что лежит на сервере.
#[derive(Debug, PartialEq, Eq)]
pub enum Opened {
    /// Расшифрованный файл.
    Encrypted(Vec<u8>),
    /// Файл не зашифрован (книжка до шифрования или правка с устройства без фразы).
    Plain(Vec<u8>),
}

impl Cipher {
    /// Новый ключ книжки и `KEY_FILE` с ним, зашифрованным фразой.
    pub fn create(passphrase: &str) -> Result<(Self, Vec<u8>)> {
        let cipher = Self { key: random()? };
        let file = cipher.wrap(passphrase)?;
        Ok((cipher, file))
    }

    /// `KEY_FILE` с этим ключом, зашифрованным фразой (соль каждый раз новая). Так меняется
    /// фраза: ключ книжки тот же, файлы перешифровывать не нужно.
    pub fn wrap(&self, passphrase: &str) -> Result<Vec<u8>> {
        let salt: [u8; 16] = random()?;
        let (memory, passes, lanes) = PARAMS;
        let wrapped = seal(&derive(passphrase, &salt, PARAMS)?, KEY_AAD, &self.key)?;
        let file = json!({
            "format": 1,
            "kdf": "argon2id",
            "memory_kib": memory,
            "passes": passes,
            "lanes": lanes,
            "salt": hex(&salt),
            "key": hex(&wrapped),
        });
        let mut text = serde_json::to_string_pretty(&file)?;
        text.push('\n');
        Ok(text.into_bytes())
    }

    /// Ключ книжки из `KEY_FILE`.
    pub fn unlock(passphrase: &str, file: &[u8]) -> Result<Self> {
        let value: Value = serde_json::from_slice(file).context("ключ книжки на сервере испорчен")?;
        if value["format"].as_u64() != Some(1) || value["kdf"].as_str() != Some("argon2id") {
            bail!("ключ книжки на сервере — незнакомого формата: обновите программу");
        }
        let number = |name: &str| value[name].as_u64().and_then(|n| u32::try_from(n).ok());
        let bytes = |name: &str| value[name].as_str().and_then(unhex);
        let (Some(memory), Some(passes), Some(lanes), Some(salt), Some(wrapped)) =
            (number("memory_kib"), number("passes"), number("lanes"), bytes("salt"), bytes("key"))
        else {
            bail!("ключ книжки на сервере испорчен");
        };
        let kek = derive(passphrase, &salt, (memory, passes, lanes))?;
        let key = open(&kek, KEY_AAD, &wrapped).context("неверная фраза шифрования")?;
        let key = key.try_into().map_err(|_| anyhow!("ключ книжки на сервере испорчен"))?;
        Ok(Self { key })
    }

    /// Файл `path` для сервера.
    pub fn seal(&self, path: &str, data: &[u8]) -> Result<Vec<u8>> {
        Ok([MAGIC, &seal(&self.key, path.as_bytes(), data)?].concat())
    }

    /// Файл `path` с сервера.
    pub fn open(&self, path: &str, data: &[u8]) -> Result<Opened> {
        match data.strip_prefix(MAGIC) {
            Some(sealed) => open(&self.key, path.as_bytes(), sealed)
                .map(Opened::Encrypted)
                .context("не расшифровывается: файл испорчен или подменён"),
            None => Ok(Opened::Plain(data.to_vec())),
        }
    }
}

/// Зашифрован ли файл с сервера.
pub fn is_sealed(data: &[u8]) -> bool {
    data.starts_with(MAGIC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        assert_eq!(unhex(&hex(b"\x00\xffab")).unwrap(), b"\x00\xffab");
        assert!(unhex("abc").is_none() && unhex("zz").is_none() && unhex("ёё").is_none());

        let (cipher, file) = Cipher::create("фраза").unwrap();
        let page = "## Работа <!-- list:work -->\n- [ ] отчёт\n".as_bytes();
        let sealed = cipher.seal("2026-10-26.md", page).unwrap();
        assert!(is_sealed(&sealed) && !sealed.windows(5).any(|w| w == b"list:"));
        // Тот же текст — каждый раз другой шифротекст (случайный nonce).
        assert_ne!(sealed, cipher.seal("2026-10-26.md", page).unwrap());

        // Другое устройство: та же фраза — тот же ключ.
        let other = Cipher::unlock("фраза", &file).unwrap();
        assert_eq!(other.open("2026-10-26.md", &sealed).unwrap(), Opened::Encrypted(page.to_vec()));
        assert_eq!(other.open("2026-10-26.md", page).unwrap(), Opened::Plain(page.to_vec()));

        // Неверная фраза, подмена страницы другой, порча.
        let err = Cipher::unlock("не та", &file).err().unwrap();
        assert!(format!("{err:#}").contains("неверная фраза"), "{err:#}");
        assert!(other.open("2026-10-27.md", &sealed).is_err());
        let mut broken = sealed.clone();
        *broken.last_mut().unwrap() ^= 1;
        assert!(other.open("2026-10-26.md", &broken).is_err());
        assert!(other.open("2026-10-26.md", MAGIC).is_err());
        assert!(Cipher::unlock("фраза", b"{}").is_err());

        // Смена фразы: старая больше не подходит, файлы читаются по-прежнему.
        let rewrapped = other.wrap("новая").unwrap();
        assert!(Cipher::unlock("фраза", &rewrapped).is_err());
        let renewed = Cipher::unlock("новая", &rewrapped).unwrap();
        assert_eq!(renewed.open("2026-10-26.md", &sealed).unwrap(), Opened::Encrypted(page.to_vec()));

        let greedy = String::from_utf8(file).unwrap().replace("\"memory_kib\": 64", "\"memory_kib\": 99999999");
        let err = Cipher::unlock("фраза", greedy.as_bytes()).err().unwrap();
        assert!(format!("{err:#}").contains("ресурсов"), "{err:#}");
    }
}
