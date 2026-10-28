//! То же окно на компьютере — для разработки, на отдельной книжке.

fn main() {
    let Some(dir) = std::env::args_os().nth(1).map(std::path::PathBuf::from) else {
        eprintln!("ztaskmgr-gui КАТАЛОГ — книжка в КАТАЛОГ, настройки в КАТАЛОГ/../config");
        std::process::exit(2);
    };
    let config = dir.with_file_name("config");
    if let Err(e) = ztaskmgr_android::run(dir, config) {
        eprintln!("ztaskmgr-gui: {e}");
        std::process::exit(1);
    }
}
