//! ztaskmgr для Android: та же книжка, что в терминале, на экране телефона. Интерфейс — Slint,
//! книжка и синхронизация — `ztaskmgr-core`. На компьютере то же окно запускается для
//! разработки: `cargo run -p ztaskmgr-android -- КАТАЛОГ`.

mod app;

pub use app::run;

/// Точка входа на Android. Книжка — во внутренней памяти приложения (`files/book`), настройки
/// устройства — рядом (`files/config`).
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(android: slint::android::AndroidApp) {
    use slint::android::android_activity::{MainEvent, PollEvent};

    let files = android.internal_data_path().unwrap_or_else(|| "/data/local/tmp/ztaskmgr".into());
    slint::android::init_with_event_listener(android, |event| match event {
        PollEvent::Main(MainEvent::Pause) => app::with_ctx(|c| c.on_pause()),
        PollEvent::Main(MainEvent::Resume { .. }) => app::with_ctx(|c| c.on_resume()),
        _ => {}
    })
    .unwrap();
    if let Err(e) = run(files.join("book"), files.join("config")) {
        eprintln!("ztaskmgr: {e}");
    }
}
