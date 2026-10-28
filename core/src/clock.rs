//! Даты страниц и часы шапки. Названия дней и месяцев — из системной локали
//! (`LC_ALL` / `LC_TIME` / `LANG`) через `strftime` из libc.

use std::ffi::CStr;
use std::fmt;

/// Формат часов в шапке: «Пн, 26 октября 2026  16:42» в ru_RU.
const CLOCK_FORMAT: &CStr = c"%a, %-d %B %Y  %H:%M";

/// Включает системную локаль для дат и часовой пояс телефона на Android. Вызывать в начале
/// `main`, пока потоков больше нет: `setlocale` и `set_var` не потокобезопасны.
pub fn init() {
    android_timezone();
    // SAFETY: пустая строка — «взять из окружения»; вызывается до запуска других потоков.
    unsafe {
        libc::setlocale(libc::LC_TIME, c"".as_ptr());
    }
}

/// Где Android хранит базу часовых поясов (новые версии — первыми).
const ANDROID_TZDATA: &[&str] = &[
    "/apex/com.android.tzdata/etc/tz/tzdata",
    "/data/misc/zoneinfo/current/tzdata",
    "/system/usr/share/zoneinfo/tzdata",
];

/// На Android часовой пояс — системное свойство, а не `TZ` и не `/etc/localtime`; статический
/// (musl) бинарник в Termux его не видит и живёт по UTC. Если `TZ` не задан, а это Android
/// (есть `getprop`; проверяется при запуске — сборка-то под `linux`), берём пояс у системы
/// и правило для него (`MSK-3`) — из системной базы поясов.
fn android_timezone() {
    if std::env::var_os("TZ").is_some_and(|tz| !tz.is_empty()) {
        return;
    }
    let getprop = std::path::Path::new("/system/bin/getprop");
    if !getprop.exists() {
        return;
    }
    let Some(name) = std::process::Command::new(getprop)
        .arg("persist.sys.timezone")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
    else {
        return;
    };
    let rule =
        ANDROID_TZDATA.iter().filter_map(|path| std::fs::read(path).ok()).find_map(|bundle| zone_rule(&bundle, &name));
    if let Some(rule) = rule {
        // SAFETY: вызывается из `init`, до запуска других потоков.
        unsafe { std::env::set_var("TZ", rule) };
    }
}

/// Правило пояса `name` (POSIX-строка вроде `MSK-3`) из базы поясов Android. База: заголовок
/// (`tzdata2024a` в 12 байтах, затем смещения таблицы, данных и zone.tab — big-endian u32),
/// таблица по 52 байта (имя в 40 байтах с нулями, начало от смещения данных, длина, запас)
/// и сами TZif-данные. Правило — последняя строка TZif версии 2 и выше.
fn zone_rule(bundle: &[u8], name: &str) -> Option<String> {
    let u32_at = |at: usize| -> Option<usize> {
        let bytes = bundle.get(at..at + 4)?;
        Some(u32::from_be_bytes(bytes.try_into().ok()?) as usize)
    };
    if !bundle.starts_with(b"tzdata") {
        return None;
    }
    let (index, data) = (u32_at(12)?, u32_at(16)?);
    let entry = (index..data).step_by(52).find(|&at| {
        bundle.get(at..at + 40).is_some_and(|raw| raw.split(|&b| b == 0).next() == Some(name.as_bytes()))
    })?;
    let (start, length) = (u32_at(entry + 40)?, u32_at(entry + 44)?);
    let tzif = bundle.get(data + start..data + start + length)?;
    // Версия 2+: «TZif2…» … «\n<правило>\n» в самом конце.
    if !tzif.starts_with(b"TZif") || tzif.get(4).is_none_or(|&v| v < b'2') {
        return None;
    }
    let body = tzif.strip_suffix(b"\n")?;
    let rule = &body[body.iter().rposition(|&b| b == b'\n')? + 1..];
    let rule = std::str::from_utf8(rule).ok()?;
    (!rule.is_empty()).then(|| rule.to_string())
}

// Часы — зависимость, которую тесты подменяют: «сейчас» в потоке теста можно остановить
// (`freeze`), и дата, метки заметок и время правки задач перестают зависеть от настоящих часов.
#[cfg(any(test, feature = "testing"))]
thread_local! {
    static FROZEN: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Тесты: остановить часы этого потока на `ms` (мс с 1970-го; `None` — снова настоящие).
#[cfg(any(test, feature = "testing"))]
pub fn freeze(ms: Option<u64>) {
    FROZEN.with(|f| f.set(ms));
}

/// Тесты: остановить часы на полдень (UTC) этой даты — дата одна и та же в любом поясе
/// от −11 до +11.
#[cfg(any(test, feature = "testing"))]
pub fn freeze_on(date: Date) {
    let days = Date::new(1970, 1, 1).unwrap().days_until(date) as u64;
    freeze(Some(days * 86_400_000 + 12 * 3_600_000));
}

/// Миллисекунды с 1970-го — время правки задачи для слияния версий.
pub fn now_ms() -> u64 {
    #[cfg(any(test, feature = "testing"))]
    if let Some(ms) = FROZEN.with(|f| f.get()) {
        return ms;
    }
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn local_tm() -> Option<libc::tm> {
    let t = (now_ms() / 1000) as libc::time_t;
    // SAFETY: `localtime_r` пишет только в переданную структуру.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        (!libc::localtime_r(&t, &mut tm).is_null()).then_some(tm)
    }
}

fn strftime(format: &CStr, tm: &libc::tm) -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `strftime` пишет не больше `buf.len()` байт и возвращает их число (0 — не поместилось).
    let len = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), format.as_ptr(), tm) };
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// Метка заметки: `26.10.2026 19:40`, не зависит от локали.
pub fn stamp() -> String {
    local_tm().map_or_else(String::new, |tm| strftime(c"%d.%m.%Y %H:%M", &tm))
}

/// Часы и минуты — для шапки на узком экране и времени последней синхронизации.
pub fn hm() -> String {
    local_tm().map_or_else(String::new, |tm| strftime(c"%H:%M", &tm))
}

/// Время с секундами: `20:46:10` — когда сохранился черновик.
pub fn time() -> String {
    local_tm().map_or_else(String::new, |tm| strftime(c"%H:%M:%S", &tm))
}

/// Текущие дата и время для шапки. Меняются раз в минуту — по смене этой строки главный цикл
/// и перерисовывает экран.
pub fn now() -> String {
    local_tm().map_or_else(String::new, |tm| strftime(CLOCK_FORMAT, &tm))
}

/// Календарная дата — страница книжки. Пишется и читается как `2026-10-26`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    year: i32,
    month: u32,
    day: u32,
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

impl Date {
    /// Дата без проверки — для констант в тестах.
    #[cfg(any(test, feature = "testing"))]
    pub const fn from_ymd(year: i32, month: u32, day: u32) -> Date {
        Date { year, month, day }
    }

    pub fn new(year: i32, month: u32, day: u32) -> Option<Date> {
        ((1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day)).then_some(Date {
            year,
            month,
            day,
        })
    }

    /// Сегодня по местному времени.
    pub fn today() -> Date {
        let tm = local_tm().expect("localtime_r");
        Date { year: tm.tm_year + 1900, month: tm.tm_mon as u32 + 1, day: tm.tm_mday as u32 }
    }

    pub fn year(self) -> i32 {
        self.year
    }

    /// Месяц: 1 — январь.
    pub fn month(self) -> u32 {
        self.month
    }

    pub fn day(self) -> u32 {
        self.day
    }

    /// День недели: 0 — понедельник, 6 — воскресенье.
    pub fn weekday(self) -> u32 {
        // 1970-01-01 — четверг.
        (self.to_days() + 3).rem_euclid(7) as u32
    }

    /// `2026-10-26`.
    pub fn parse_iso(text: &str) -> Option<Date> {
        let mut parts = text.splitn(3, '-');
        let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
        if y.len() != 4 || m.len() != 2 || d.len() != 2 {
            return None;
        }
        Date::new(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
    }

    /// Дата, как её вводят руками: `2026-10-27`, `27.10.2026`, `27.10` (год — от `base`),
    /// `+1` / `-3` (дни от `base`).
    pub fn parse_input(text: &str, base: Date) -> Option<Date> {
        let text = text.trim();
        if let Some(n) = text.strip_prefix('+') {
            return Some(base.add_days(n.parse().ok()?));
        }
        if let Some(n) = text.strip_prefix('-').filter(|n| n.chars().all(|c| c.is_ascii_digit())) {
            return Some(base.add_days(-n.parse::<i64>().ok()?));
        }
        if let Some(date) = Date::parse_iso(text) {
            return Some(date);
        }
        let mut parts = text.split('.');
        let day = parts.next()?.parse().ok()?;
        let month = parts.next()?.parse().ok()?;
        let year = match parts.next() {
            Some(y) if !y.is_empty() => y.parse().ok()?,
            _ => base.year,
        };
        if parts.next().is_some() {
            return None;
        }
        Date::new(year, month, day)
    }

    /// Номер дня от 1970-01-01 (алгоритм days_from_civil Говарда Хиннанта).
    fn to_days(self) -> i64 {
        let (m, d) = (i64::from(self.month), i64::from(self.day));
        let y = i64::from(self.year) - i64::from(m <= 2);
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    fn from_days(days: i64) -> Date {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
        Date { year, month, day }
    }

    pub fn add_days(self, n: i64) -> Date {
        Date::from_days(self.to_days() + n)
    }

    /// Сколько дней от `self` до `other` (отрицательное — `other` раньше).
    pub fn days_until(self, other: Date) -> i64 {
        other.to_days() - self.to_days()
    }

    /// Дата по формату `strftime` в системной локали.
    pub fn format(self, format: &CStr) -> String {
        let days = self.to_days();
        // SAFETY: tm — простая структура из чисел, нули допустимы.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = self.year - 1900;
        tm.tm_mon = self.month as i32 - 1;
        tm.tm_mday = self.day as i32;
        // 1970-01-01 — четверг; в tm воскресенье — 0.
        tm.tm_wday = (days + 4).rem_euclid(7) as i32;
        tm.tm_yday = (days - Date { month: 1, day: 1, ..self }.to_days()) as i32;
        strftime(format, &tm)
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekday() {
        let d = Date::new(2026, 10, 26).unwrap();
        assert_eq!((d.weekday(), d.month(), d.day()), (0, 10, 26));
        assert_eq!(d.add_days(6).weekday(), 6);
        assert_eq!(Date::new(1970, 1, 1).unwrap().weekday(), 3);
        assert_eq!(Date::new(1969, 12, 29).unwrap().weekday(), 0);
    }

    fn d(y: i32, m: u32, day: u32) -> Date {
        Date::new(y, m, day).unwrap()
    }

    /// База поясов Android из одной-двух зон, как её собирает ZoneCompactor.
    fn bundle(zones: &[(&str, &[u8])]) -> Vec<u8> {
        let index = 24;
        let data = index + 52 * zones.len();
        let mut out = b"tzdata2026a\0".to_vec();
        out.extend((index as u32).to_be_bytes());
        out.extend((data as u32).to_be_bytes());
        out.extend(0u32.to_be_bytes());
        let mut start = 0;
        for (name, tzif) in zones {
            let mut raw = [0u8; 40];
            raw[..name.len()].copy_from_slice(name.as_bytes());
            out.extend(raw);
            out.extend((start as u32).to_be_bytes());
            out.extend((tzif.len() as u32).to_be_bytes());
            out.extend(0u32.to_be_bytes());
            start += tzif.len();
        }
        for (_, tzif) in zones {
            out.extend(*tzif);
        }
        out
    }

    #[test]
    fn android_zone_rule() {
        let moscow: &[u8] = b"TZif2\0\0\0 v1 data TZif2 v2 data\nMSK-3\n";
        let berlin: &[u8] = b"TZif3 data\nCET-1CEST,M3.5.0,M10.5.0/3\n";
        let base = bundle(&[("Europe/Berlin", berlin), ("Europe/Moscow", moscow)]);
        assert_eq!(zone_rule(&base, "Europe/Moscow").as_deref(), Some("MSK-3"));
        assert_eq!(zone_rule(&base, "Europe/Berlin").as_deref(), Some("CET-1CEST,M3.5.0,M10.5.0/3"));
        // Префикс имени — не та зона.
        assert_eq!(zone_rule(&base, "Europe/Mos"), None);
        assert_eq!(zone_rule(&base, "Asia/Tokyo"), None);
        // TZif версии 1 — правила нет; не база — ничего.
        assert_eq!(zone_rule(&bundle(&[("UTC", b"TZif\0 old")]), "UTC"), None);
        assert_eq!(zone_rule(b"not a bundle", "UTC"), None);
        assert_eq!(zone_rule(&base[..30], "Europe/Moscow"), None);
    }

    /// Настоящая база Android: `ZTASKMGR_TZDATA=…/tzdata cargo test -- --ignored android_real`.
    #[test]
    #[ignore]
    fn android_real_bundle() {
        let path = std::env::var("ZTASKMGR_TZDATA").expect("путь к tzdata в ZTASKMGR_TZDATA");
        let base = std::fs::read(path).unwrap();
        for (zone, rule) in [
            ("Europe/Moscow", "MSK-3"),
            ("Asia/Yekaterinburg", "<+05>-5"),
            ("Europe/Berlin", "CET-1CEST,M3.5.0,M10.5.0/3"),
            ("America/New_York", "EST5EDT,M3.2.0,M11.1.0"),
        ] {
            assert_eq!(zone_rule(&base, zone).as_deref(), Some(rule), "{zone}");
        }
    }

    #[test]
    fn formats_current_time() {
        // В тестах локаль не включается: «C», английские названия.
        let text = now();
        assert!(text.contains(&Date::today().year().to_string()), "{text}");
        assert!(text.contains(':') && text.contains(", "), "{text}");
        let stamp = stamp();
        assert!(stamp.starts_with(&Date::today().format(c"%d.%m.%Y")), "{stamp}");
        assert_eq!(stamp.len(), 16);
    }

    #[test]
    fn day_arithmetic() {
        assert_eq!(d(1970, 1, 1).to_days(), 0);
        assert_eq!(d(2026, 10, 26).add_days(6), d(2026, 11, 1));
        assert_eq!(d(2024, 2, 28).add_days(1), d(2024, 2, 29));
        assert_eq!(d(2026, 3, 1).add_days(-1), d(2026, 2, 28));
        assert_eq!(d(2026, 12, 31).add_days(1), d(2027, 1, 1));
        assert_eq!(d(1969, 12, 31).to_days(), -1);
        assert_eq!(d(2026, 10, 26).days_until(d(2026, 10, 24)), -2);
        for days in -800_000..800_000 {
            if days % 997 == 0 {
                assert_eq!(Date::from_days(days).to_days(), days);
            }
        }
        assert_eq!(Date::new(2026, 2, 29), None);
        assert_eq!(Date::new(2026, 13, 1), None);
    }

    #[test]
    fn parse() {
        let base = d(2026, 10, 26);
        assert_eq!(Date::parse_iso("2026-10-27"), Some(d(2026, 10, 27)));
        assert_eq!(Date::parse_iso("2026-1-27"), None);
        assert_eq!(Date::parse_input("2026-10-27", base), Some(d(2026, 10, 27)));
        assert_eq!(Date::parse_input("27.10.2027", base), Some(d(2027, 10, 27)));
        assert_eq!(Date::parse_input(" 1.11 ", base), Some(d(2026, 11, 1)));
        assert_eq!(Date::parse_input("+7", base), Some(d(2026, 11, 2)));
        assert_eq!(Date::parse_input("-26", base), Some(d(2026, 9, 30)));
        assert_eq!(Date::parse_input("31.02", base), None);
        assert_eq!(Date::parse_input("завтра", base), None);
        assert_eq!(Date::parse_input("", base), None);
        assert_eq!(d(2026, 1, 5).to_string(), "2026-01-05");
    }

    #[test]
    fn format_in_c_locale() {
        assert_eq!(d(2026, 10, 26).format(c"%A %-d %B %j"), "Monday 26 October 299");
        assert_eq!(d(2026, 11, 1).format(c"%a"), "Sun");
    }
}
