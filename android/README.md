# ztaskmgr для Android

Та же книжка, что в терминале, на экране телефона: интерфейс на [Slint](https://slint.dev),
книжка, слияние и синхронизация — общее ядро `ztaskmgr-core`. Всё на Rust, без Kotlin и Gradle.

## Сборка

Нужны Rust-таргет, Android SDK и NDK, `cargo-apk` и `javac` (JDK 17+):

```sh
rustup target add aarch64-linux-android
cargo install cargo-apk

# SDK и NDK — через Android CLI из commandlinetools (https://developer.android.com/studio#command-line-tools-only),
# распакованных в ~/Android/Sdk/cmdline-tools/latest:
~/Android/Sdk/cmdline-tools/latest/bin/android --sdk=$HOME/Android/Sdk sdk install \
    platforms/android-36 build-tools/36.1.0 ndk/30.0.16248370

export ANDROID_HOME=$HOME/Android/Sdk
export ANDROID_NDK_ROOT=$ANDROID_HOME/ndk/30.0.16248370
cd android
cargo apk build --target aarch64-linux-android --lib            # target/debug/apk/ztaskmgr.apk
adb install -r ../target/debug/apk/ztaskmgr.apk
```

Release-сборке нужен ключ подписи: `CARGO_APK_RELEASE_KEYSTORE` и
`CARGO_APK_RELEASE_KEYSTORE_PASSWORD` (или `[package.metadata.android.signing.release]`).
Обновление APK ставится поверх только с тем же ключом.

```sh
cargo apk build --release --target aarch64-linux-android --lib  # ≈ 10 МБ
```

Для разработки то же окно запускается на компьютере, на отдельной книжке:

```sh
cargo run -p ztaskmgr-android -- /tmp/книжка    # настройки — /tmp/config
```

## Как устроено

- Книжка — во внутренней памяти приложения (`files/book`), настройки устройства — рядом
  (`files/config`, тот же формат, что `~/.config/ztaskmgr/config`). Termux-версию и её книжку
  приложение не трогает; общая у них только книжка на сервере WebDAV.
- Экраны: страница дня (списки — вкладки, свайп влево-вправо — соседний день, тап по дате —
  сегодня), задача (текст, выполнено, отменить, перенести, в другой список, выше/ниже,
  удалить), записи (заметки и подзадачи), настройки (WebDAV, фраза шифрования, смена фразы).
- Синхронизация — как в программе для терминала: при запуске, через пару секунд после правки,
  раз в минуту, при уходе в фон и возвращении. В шапке — `⇅ 14:04` или `⇅ ✗`.
- Дни и месяцы — по-русски своей таблицей: в Android нет системной локали.
- «О программе» (в настройках) показывает `AboutSlint` — условие бесплатной лицензии Slint (Royalty-free 2.0).
