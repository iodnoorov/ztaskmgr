//! Дата и время для шапки в системной локали (`LC_ALL` / `LC_TIME` / `LANG`): названия дней
//! и месяцев даёт `strftime` из libc.

use std::ffi::CStr;

/// Формат шапки: «пн, 26 октября 2026  16:42» в ru_RU, «Mon, 26 October 2026  16:42» в en_US.
const FORMAT: &CStr = c"%a, %-d %B %Y  %H:%M";

/// Включает системную локаль для дат. Вызывать в начале `main`, пока потоков больше нет:
/// `setlocale` не потокобезопасна.
pub fn init() {
    // SAFETY: пустая строка — «взять из окружения»; вызывается до запуска других потоков.
    unsafe {
        libc::setlocale(libc::LC_TIME, c"".as_ptr());
    }
}

/// Текущие дата и время. Меняются раз в минуту — по смене этой строки главный цикл
/// и перерисовывает экран.
pub fn now() -> String {
    let mut buf = [0u8; 128];
    // SAFETY: `time` и `localtime_r` пишут только в переданные структуры; `strftime` пишет не
    // больше `buf.len()` байт и возвращает их число (0 — не поместилось).
    let len = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        libc::strftime(buf.as_mut_ptr().cast(), buf.len(), FORMAT.as_ptr(), &tm)
    };
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_current_time() {
        // В тестах локаль не включается: «C», английские названия.
        let text = now();
        let year = unsafe { libc::time(std::ptr::null_mut()) } / 31_556_952 + 1970;
        assert!(text.contains(&year.to_string()), "{text}");
        assert!(text.contains(':') && text.contains(", "), "{text}");
    }
}
