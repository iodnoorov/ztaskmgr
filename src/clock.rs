//! Даты страниц и часы шапки. Названия дней и месяцев — из системной локали
//! (`LC_ALL` / `LC_TIME` / `LANG`) через `strftime` из libc.

use std::ffi::CStr;
use std::fmt;

/// Формат часов в шапке: «Пн, 26 октября 2026  16:42» в ru_RU.
const CLOCK_FORMAT: &CStr = c"%a, %-d %B %Y  %H:%M";

/// Включает системную локаль для дат. Вызывать в начале `main`, пока потоков больше нет:
/// `setlocale` не потокобезопасна.
pub fn init() {
    // SAFETY: пустая строка — «взять из окружения»; вызывается до запуска других потоков.
    unsafe {
        libc::setlocale(libc::LC_TIME, c"".as_ptr());
    }
}

fn local_tm() -> Option<libc::tm> {
    // SAFETY: `time` и `localtime_r` пишут только в переданные структуры.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
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

/// Метка заметки: `2026-10-26 19:40`, не зависит от локали.
pub fn stamp() -> String {
    local_tm().map_or_else(String::new, |tm| strftime(c"%Y-%m-%d %H:%M", &tm))
}

/// Часы и минуты — для шапки на узком экране.
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
    pub fn new(year: i32, month: u32, day: u32) -> Option<Date> {
        ((1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day))
            .then_some(Date { year, month, day })
    }

    /// Сегодня по местному времени.
    pub fn today() -> Date {
        let tm = local_tm().expect("localtime_r");
        Date {
            year: tm.tm_year + 1900,
            month: tm.tm_mon as u32 + 1,
            day: tm.tm_mday as u32,
        }
    }

    pub fn year(self) -> i32 {
        self.year
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

    fn d(y: i32, m: u32, day: u32) -> Date {
        Date::new(y, m, day).unwrap()
    }

    #[test]
    fn formats_current_time() {
        // В тестах локаль не включается: «C», английские названия.
        let text = now();
        assert!(text.contains(&Date::today().year().to_string()), "{text}");
        assert!(text.contains(':') && text.contains(", "), "{text}");
        let stamp = stamp();
        assert!(stamp.starts_with(&Date::today().to_string()), "{stamp}");
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
