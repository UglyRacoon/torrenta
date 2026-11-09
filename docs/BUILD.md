# Сборка Torrenta под Windows

Инструкция для тех, кто собирает приложение из исходников: что установить, как запустить сборку и что делать, если что-то пошло не так. Краткая версия — в разделе [«Быстрый старт»](../README.md#быстрый-старт) README.

## 1. Что потребуется

| Компонент | Версия | Зачем |
| --- | --- | --- |
| Windows | 10 или 11, x64 | Целевая платформа; ARM и x86 не поддерживаются |
| Visual Studio Build Tools 2022 | с нагрузкой «Разработка классических приложений на C++» (MSVC + Windows SDK) | Компоновка Rust-кода под MSVC (`link.exe`) |
| Rust | стабильный тулчейн **1.77+**, `stable-x86_64-pc-windows-msvc` | Сборка ядра и приложения |
| Node.js | **20.19+** или 22 LTS/новее (требование Vite 7) | Сборка интерфейса |
| WebView2 Runtime | Evergreen | Окно приложения (в Windows 11 уже есть) |
| Google Chrome | любая актуальная | Только для E2E-тестов интерфейса (раздел 6); для сборки не нужен |

Опционально:

- **WebView2 Runtime** нужен и для сборки (`tauri build` проверяет его наличие), и для запуска. Установщик Torrenta умеет доставлять его сам (режим `embedBootstrapper`, тихая установка).

Все зависимости ставятся бесплатно и без аккаунтов.

В 1.7.0 версии инструментов из таблицы не изменились: по-прежнему стабильный Rust **1.77+** с MSVC и Node.js **20.19+**. Изменился состав зависимостей. В Cargo-графе появился крейт `tauri-plugin-updater` (и его транзитивные зависимости) — его компилирует первая сборка 1.7.0, дальше он лежит в кэше; `windows-sys` был в графе и раньше, добавлены только `features` для работы с реестром (самолечение ассоциации). Во фронтенде добавился devDependency `@playwright/test` — он нужен только для E2E (раздел 6) и в бандл приложения не попадает. Всё остальное в 1.7.0 — собственный код: `association.rs`, расписание лимитов, последовательная загрузка, множественная сортировка, `check_update`.

### 1.1 Установка через winget

```powershell
winget install Microsoft.VisualStudio.2022.BuildTools `
  --override "--wait --quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
winget install Rustlang.Rustup
winget install OpenJS.NodeJS.LTS
winget install Microsoft.EdgeWebView2Runtime
```

После установки Rust нужно один раз выбрать MSVC-тулчейн:

```powershell
rustup default stable-x86_64-pc-windows-msvc
rustup show
```

`rustup show` должен показать активным `stable-x86_64-pc-windows-msvc`. Если единственный тулчейн — `stable-x86_64-pc-windows-gnu`, сборка упадёт на этапе компоновки (см. [раздел 5](#5-типичные-ошибки)).

### 1.2 Без winget

- **Build Tools:** [visualstudio.microsoft.com/vs/community](https://visualstudio.microsoft.com/vs/community/) → установщик Visual Studio Installer → вкладка «Отдельные компоненты» → «MSVC v143 — VS 2022 C++ x64/x86 build tools» и «Windows 11 SDK».
- **Rust:** `winget`-альтернатива — установщик [rustup-init.exe](https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe) для x86_64-pc-windows-msvc.
- **Node.js:** [nodejs.org](https://nodejs.org/) → LTS-сборка для Windows x64.
- **WebView2:** [developer.microsoft.com/microsoft-edge/webview2](https://developer.microsoft.com/microsoft-edge/webview2/) → Evergreen Bootstrapper.

### 1.3 Проверка окружения

Откройте новый терминал (после установки обновился `PATH`) и проверьте:

```powershell
rustc -V                              # rustc 1.xx.x
cargo -V                              # cargo 1.xx.x
rustup show                           # активен ...-msvc
node -v                               # v20.19 или новее
npm -v
```

`cl.exe` в `PATH` обычно **нет** даже при установленных Build Tools — это нормально: `rustc` находит MSVC сам. Сборка через `build-windows.ps1` ищет установку через `vswhere.exe` (`C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe`).

## 2. Сборка скриптом

Из корня репозитория:

```powershell
.\build-windows.bat
```

или напрямую:

```powershell
powershell -ExecutionPolicy Bypass -File .\build-windows.ps1
```

`build-windows.bat` — тонкая обёртка: переходит в свой каталог, запускает `.ps1` с теми же аргументами, а при ошибке показывает код возврата и ждёт нажатия клавиши (удобно при запуске двойным щелчком).

### 2.1 Параметры

| Параметр | Что делает |
| --- | --- |
| `-Help` | печатает справку и выходит с кодом 0 |
| `-SkipTests` | не запускать `cargo test` |
| `-NoInstall` | ничего не устанавливать: только проверить окружение и собрать с тем, что есть |
| `-Dev` | запустить `npm run tauri dev` вместо релизной сборки |
| `-Clean` | `cargo clean` и удаление `node_modules` и `dist` перед сборкой |
| `-BundleOnly` | только установщик NSIS: `npm run tauri build -- --bundles nsis` |

Примеры:

```powershell
# проверить окружение на машине без права установки пакетов
.\build-windows.ps1 -NoInstall -SkipTests

# полная пересборка с нуля
.\build-windows.ps1 -Clean

# быстрый запуск в режиме разработки
.\build-windows.ps1 -Dev
```

### 2.2 Что делает скрипт

1. Проверяет, что запуск идёт из корня репозитория, и что ОС — Windows x64.
2. Проверяет MSVC (через `vswhere` или `cl.exe` в `PATH`) и WebView2 Runtime; без MSVC печатает подсказку по установке и выходит с ошибкой.
3. Проверяет `rustup`, `cargo`, активный MSVC-тулчейн и версию Rust (не ниже 1.77). При `-NoInstall` только предупреждает и не меняет тулчейн по умолчанию.
4. Проверяет Node.js 20.19+ и npm.
5. Ставит зависимости фронтенда: `npm ci` (при отсутствии `package-lock.json` — `npm install`).
6. Запускает `npm run check:contrast` и `npm run build` (при `-Dev` и `-BundleOnly` отдельная сборка интерфейса не нужна — её выполняет сам `tauri`).
7. Запускает `cargo test --workspace` (если не указан `-SkipTests`).
8. Запускает `npm run tauri build` (или `npm run tauri dev` при `-Dev`, или только NSIS при `-BundleOnly`).
9. Печатает пути, размеры файлов из `target\release\bundle`, общее время сборки и код возврата.

Все сообщения — на русском. Скрипт рассчитан на Windows PowerShell 5.1 и PowerShell 7; запускайте его с `-ExecutionPolicy Bypass` (именно так делает `build-windows.bat`), тогда менять политику выполнения в системе не нужно.

E2E-тесты скрипт не запускает: им нужен Google Chrome и отдельный `vite preview` на 4173, а не окно Tauri (раздел 6). Прогоняйте их вручную — `npm run test:e2e`.

## 3. Ручная сборка

Если скрипт не нужен:

```powershell
npm ci
npm run check:contrast
npm run build
cargo test --workspace
npm run tauri build
```

Или через CLI Tauri напрямую:

```powershell
npm run tauri -- build
```

Артефакты:

| Что | Где |
| --- | --- |
| Исполняемый файл | `target\release\torrenta.exe` |
| Установщик NSIS | `target\release\bundle\nsis\Torrenta_<версия>_x64-setup.exe` |
| Установщик MSI | `target\release\bundle\msi\Torrenta_<версия>_x64_*.msi` |

`<версия>` — поле `version` в `src-tauri\tauri.conf.json`; отдельно её нигде указывать не нужно, Tauri берёт имя файла оттуда.

Порядок и хуки: перед сборкой Rust Tauri сам запускает `beforeBuildCommand` из `src-tauri\tauri.conf.json` — то есть `npm run build`; результат попадает в `dist` (`frontendDist`) и встраивается в бинарник. Поэтому отдельный `npm run build` перед `tauri build` не обязателен: он нужен только для проверки типов и размера бандла без сборки приложения.

Ассоциация `.torrent` объявлена в `bundle.fileAssociations` и создаётся только установщиком, поэтому `cargo build`/`cargo check` её не проверяют. После установки NSIS-версии стоит убедиться, что ключи появились и команда запуска содержит кавычки вокруг `%1`:

```powershell
Get-ItemProperty 'HKCU:\Software\Classes\.torrent'
Get-ItemProperty 'HKCU:\Software\Classes\Torrenta.torrent\shell\open\command'
```

Ожидаемая команда: `"C:\Users\<вы>\AppData\Local\Torrenta\torrenta.exe" "%1"`. Если ключей нет (WiX пишет рекламируемые ключи, которые Windows создаёт при установке компонента, поэтому MSI-пакет стоит проверять отдельно), закрепите Torrenta через «Открыть с помощью → Выбрать другое приложение». Проверка единственного экземпляра — запустить `torrenta.exe` дважды: второго окна быть не должно, окно первого поднимается, а второй процесс исчезает из диспетчера задач.

Отдельная ловушка сборки: описание файловой ассоциации в `tauri.conf.json` должно быть латиницей. WiX генерирует `main.wxs` с кодовой страницей 1252, и кириллица в `ProgId/@Description` валит MSI (`LGHT0311`), хотя NSIS при этом собирается. Поэтому в конфигурации стоит `"description": "Torrent file"`.

Для быстрой итерации по интерфейсу без пересборки Rust:

```powershell
npm run dev            # Vite на http://localhost:5173 (порт фиксирован: strictPort)
npm run tauri:dev      # окно Tauri с горячей перезагрузкой
```

## 4. Куда уходит время и место

- Первая сборка самая долгая: компилируется весь граф зависимостей (Tauri, tokio, rustls). Дальше работает инкрементальная компиляция.
- Прирост 1.7.0 — это в основном собственный код (`association.rs`, расписание лимитов, последовательная загрузка, множественная сортировка), но в графе появился `tauri-plugin-updater`. Его компиляция удлиняет первую сборку после обновления; крейты вроде `tauri`, `tokio` и `rustls` были и раньше, поэтому дальше пересобираются только затронутые файлы. Оценки времени — ориентиры с прежних сборок, заново на 1.7.0 они не измерялись.
- `target\` в релизной сборке занимает несколько гигабайт; `-Clean` освобождает их, но следующая сборка снова будет полной.
- `node_modules\` — сотни мегабайт; удаляется вместе с `dist` флагом `-Clean`.
- Установщики NSIS/MSI скачиваются один раз в `%LOCALAPPDATA%\tauri` и переиспользуются.

## 5. Типичные ошибки

### 5.1 `error: linker 'link.exe' not found`

```
error: linker `link.exe` not found
  |
  = note: the msvc targets depend on the msvc linker but `link.exe` was not found
```

**Причина:** не установлены MSVC Build Tools, либо активен GNU-тулчейн.

**Решение:**

1. Установить Build Tools 2022 с нагрузкой C++ (см. раздел 1).
2. Выбрать правильный тулчейн: `rustup default stable-x86_64-pc-windows-msvc`.
3. Открыть новый терминал (после установки обновился `PATH`) и повторить сборку.

`cl.exe`, `link.exe` и `nmake.exe` не обязаны быть в `PATH` — их находит `rustc`. Если ошибка осталась, проверьте `rustup show`: в выводе должен быть `stable-x86_64-pc-windows-msvc`.

### 5.2 `error[E0463]: can't find crate for 'core'` / `cannot find -lwindows`

**Причина:** сборка идёт под GNU-тулчейном или смешаны тулчейны после переустановки Rust.

**Решение:**

```powershell
rustup default stable-x86_64-pc-windows-msvc
rustup target list --installed
cargo clean
```

### 5.3 Ошибка про Windows SDK / `windows.lib` при сборке `windows`-крейта

**Причина:** Build Tools установлены без Windows SDK.

**Решение:** Visual Studio Installer → «Изменение» → «Отдельные компоненты» → Windows 10/11 SDK.

### 5.4 Окно пустое или «WebView2 runtime is not installed»

**Причина:** нет WebView2 Runtime (часто на «чистой» Windows 10 или в урезанных сборках).

**Решение:**

```powershell
winget install Microsoft.EdgeWebView2Runtime
```

или установить Evergreen Runtime с сайта Microsoft. WebView2 ставится на пользователя или на всю машину; для сборки установщика с доставкой рантайма используется `embedBootstrapper` в `src-tauri\tauri.conf.json` (уже настроено).

### 5.5 `os error 32` / `os error 5` при сборке, «файл занят»

**Причина:** запущенный `torrenta.exe` (или антивирус) держит файлы в `target\`.

**Решение:**

1. Закройте приложение (в том числе из трея — проверьте Диспетчер задач).
2. Повторите сборку.
3. Если это антивирус: добавьте каталог проекта в исключения.

### 5.6 Антивирус замедляет или блокирует сборку

**Причина:** сканирование `target\` при каждой записи тысяч объектных файлов.

**Решение:** добавить в исключения Windows Defender (или стороннего антивируса) каталоги `target\`, `node_modules\` и `%USERPROFILE%\.cargo`. Это стандартная мера для Rust-проектов на Windows.

### 5.7 Длинные пути: `error: could not create ...: The system cannot find the path specified`

**Причина:** классическое ограничение в 260 символов; Rust-проекты с глубоким `target\` легко его превышают.

**Решение (любое из):**

1. Включить поддержку длинных путей:

   ```powershell
   Set-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem' -Name LongPathsEnabled -Value 1
   ```

   (требуются права администратора и перезагрузка; в Windows 11 часто включено по умолчанию)

2. Клонировать репозиторий ближе к корню: `C:\dev\torrenta`, а не `C:\Users\...\Documents\Projects\...`.
3. Для Git: `git config --global core.longpaths true`.

### 5.8 `npm ci` падает: нет `package-lock.json` или `ERESOLVE`

**Причина:** lock-файл отсутствует, повреждён или версия npm несовместима.

**Решение:** обновить npm (`npm install -g npm@latest`) и повторить; если lock-файла нет, скрипт сам откатывается на `npm install`. Конфликт peer-зависимостей означает, что кто-то поменял `package.json` без обновления lock-файла — восстановите `package-lock.json` из репозитория.

### 5.9 `npm run check:contrast` падает

**Причина:** изменилась палитра в `src\styles\tokens.css` и какая-то пара «текст/фон» больше не проходит по контрасту.

**Решение:** запустите `npm run check:contrast` и посмотрите, какие пары и с каким коэффициентом названы в выводе; поправьте значения токенов. Полностью отключать проверку не нужно — она часть CI.

### 5.10 `running scripts is disabled on this system`

**Причина:** политика выполнения PowerShell запрещает `.ps1`.

**Решение:** не менять политику глобально, а запускать с обходом:

```powershell
powershell -ExecutionPolicy Bypass -File .\build-windows.ps1
```

Именно так делает `build-windows.bat`.

### 5.11 `cargo`/`rustup` не найдены после установки

**Причина:** `PATH` не обновился в текущем процессе.

**Решение:** откройте новый терминал. Если не помогло, проверьте `%USERPROFILE%\.cargo\bin` в `PATH` (туда `rustup` кладёт `cargo.exe`, `rustc.exe`, `rustup.exe`).

### 5.12 Ошибка загрузки toolchain: `could not download file ... certificate verify failed`

**Причина:** корпоративный прокси, TLS-инспекция или блокировка `static.rust-lang.org`.

**Решение:** настроить прокси (`HTTPS_PROXY`/`HTTP_PROXY`), обновить сертификаты Windows, либо поставить MSVC Build Tools и Rust вручную (раздел 1.2). В PowerShell 5.1 скрипт сам включает TLS 1.2, поэтому ошибки TLS обычно означают прокси между вами и сервером.

### 5.13 `failed to bundle project` (NSIS/MSI)

**Причина:** не скачались утилиты упаковки (`makensis`, WiX), обычно из-за прокси или блокировки сети.

**Решение:**

1. Повторить сборку: загрузка кэшируется в `%LOCALAPPDATA%\tauri`.
2. Собрать только NSIS: `.\build-windows.ps1 -BundleOnly`.
3. Проверить, что `Torrenta_<версия>_x64-setup.exe` не запущен в этот момент.

### 5.14 `error: package torrent-core v1.7.0 ... requires rustc 1.77`

**Причина:** устаревший Rust.

**Решение:** `rustup update stable`, затем `rustc -V`.

### 5.15 `npm run check:contrast` не проходит после добавления цвета в настройки

```
  ✗ Провалено пар: 1 из 68
    — [dark] акцентный текст / карточка: 4.12:1 (нужно ≥ 4.5), #5EEAD4 на #1A2130
```

**Причина:** токен добавлен или изменён в `src\styles\tokens.css`, и какая-то обязательная пара «текст/фон» больше не дотягивает до WCAG AA. Скрипт берёт **34 пары** и проверяет каждую в обеих темах (тёмная и светлая) — всего **68 проверок**: обычный текст не ниже 4.5:1, крупный текст и нетекстовые элементы (иконки статусов, фокус-ринг, границы контролов) — не ниже 3:1.

**Решение:**

1. Найдите в выводе строку `— [тема] пара: коэффициент (нужно ≥ норма)` и поправьте сам токен: на тёмной теме текст обычно светлее, на светлой — темнее. Обе темы лежат в одном файле, блоками `/* @theme dark */` и `/* @theme light */`, поэтому цвет нужно согласовать в двух местах.
2. Если добавляете новый элемент интерфейса (например, ещё один бейдж), допишите его пару в массив `PAIRS` в `scripts\check-contrast.mjs` — иначе проверка новый токен просто не увидит и пропустит.
3. Порог не снижайте и проверку не отключайте: она входит в CI и в обязательные гейты (раздел 9).

### 5.16 UPnP/NAT-PMP: `cargo test -p torrent-core -- --ignored` падает без роутера

**Причина:** живой проброс порта в автотестах помечен `#[ignore = "нужна реальная сеть с роутером, поддерживающим UPnP/NAT-PMP"]` (`crates\torrent-core\src\port_map.rs`). Поэтому обычный `cargo test --workspace` его не запускает — в отчёте это и есть те самые 1 ignored.

**Решение:** ничего не чинить — это не ошибка сборки. Запускать игнорируемые тесты имеет смысл только на машине, где роутер действительно поддерживает UPnP или NAT-PMP:

```powershell
cargo test -p torrent-core -- --ignored
```

Без такого роутера тест упадёт по таймауту — так и задумано.

### 5.17 Тест полез в настоящую сеть

**Причина:** собственный конфиг движка собран без отключения DHT. Во всех интеграционных конфигах (`crates\torrent-core\tests\*.rs`, 8 наборов, из них 7 поднимают движок, а `bencode_fuzz` работает только с кодеком) стоит `dht: false`, поэтому тесты видят лишь локальный mock-трекер и не зависят от интернета.

**Решение:** в новом конфиге тоже выключите DHT — как в существующих:

```rust
EngineConfig {
    download_dir: download_dir.to_path_buf(),
    listen_port: 0,
    // DHT ходит в настоящую сеть — в тестах он не нужен и только шумит.
    dht: false,
    ..EngineConfig::default()
}
```

### 5.18 E2E падают: `Chromium distribution 'chrome' is not found`

**Причина:** в `playwright.config.ts` для проекта `chrome` указан `channel: "chrome"` — это системный Google Chrome, а не браузер, скачанный Playwright. Если Chrome не установлен (или установлен в нестандартное место), такой браузер не запускается.

**Решение:** поставить обычный Google Chrome. Альтернатива — переключить проект на скачанный Chromium: поставить `channel: "chromium"` в `playwright.config.ts` и скачать браузер один раз через `npx playwright install chromium`. Держать оба варианта одновременно не стоит: набор должен одинаково работать у всех, а системный Chrome уже есть почти везде.

### 5.19 E2E проходят «по-старому» или падают сразу после правок интерфейса

**Причина:** `vite preview` раздаёт каталог `dist\`, а не исходники. Если после правок в `src\` не запускали `npm run build`, тесты проверяют предыдущий бандл: старые проверки проходят, новые — падают.

**Решение:** прогонять `npm run build` перед каждым `npm run test:e2e` (или `npx playwright test`). Проверить, что сборка свежая, можно по времени изменения `dist\index-*.js`. Второй источник «пустого» экрана — не запущенный сервер: `webServer` в конфиге поднимает `npm run preview` сам, но если порт 4173 занят чужим процессом, прогон упадёт с таймаутом.

## 6. Сквозные тесты интерфейса (E2E)

В `e2e\` лежат сценарии [Playwright](https://playwright.dev) на пользовательские пути: список торрентов, поиск и фильтры, настройки, сортировка, редактор трекеров, диагностика. Tauri и WebView2 для них не нужны: тесты открывают интерфейс в браузере против `vite preview`, а он раздаёт собранный `dist\`. В браузере `isTauriRuntime()` из `src\api\engine.ts` не находит `__TAURI_INTERNALS__`, приложение переключается на демо-АПИ (`src\api\mock.ts`) — тесты видят фиксированные демо-торренты и не зависят от сети, движка и установленного приложения.

Конфигурация — [playwright.config.ts](../playwright.config.ts):

| Настройка | Значение | Зачем так |
| --- | --- | --- |
| `testDir` | `./e2e` | сценарии рядом с приложением, по файлу на фичу |
| `channel` | `"chrome"` | системный Google Chrome: браузеры Playwright не скачиваются |
| `webServer` | `npm run preview` на `http://localhost:4173` | `vite preview` раздаёт `dist\`; занятый сервер переиспользуется (`reuseExistingServer`) |
| `workers` / `fullyParallel` | `1` / `false` | демо-АПИ живёт в памяти страницы, поэтому тесты идут строго по одному |
| `locale` / `viewport` | `ru-RU` / 1440×900 | тексты и раскладка как в поставке |
| `trace` / `screenshot` | `retain-on-failure` / `only-on-failure` | файлы пишутся только для упавших тестов |

Запуск:

```powershell
npm run build          # обязательно: preview раздаёт dist, а не src
npm run test:e2e       # то же, что npx playwright test
npx playwright test e2e\settings.spec.ts   # один файл
npx playwright test -g "ассоциация"       # по названию теста
npx playwright test --reporter=list       # без HTML-отчёта, строкой на тест
npx playwright test --ui                  # интерактивный режим
```

Отчёты и артефакты: `playwright-report\` (HTML, открывается `npx playwright show-report`), `test-results\` (трейсы и скриншоты упавших тестов). Оба каталога в `.gitignore`.

Требования: Node.js из раздела 1 и установленный Google Chrome. Переменная `PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1` запрещает Playwright скачивать браузеры — её проверяют и установка через npm, и сам CLI, так что `npx playwright install` с ней ничего не загрузит. Нужна она только на стендах, где запрещено тянуть сотни мегабайт браузеров: для прогона достаточно системного Chrome.

Сейчас в наборе **12 сценариев**, полный прогон занимает около 9 секунд:

| Файл | Сценариев | Что проверяет |
| --- | --- | --- |
| `e2e\list.spec.ts` | 4 | демо-список, поиск, фильтры и счётчики в сайдбаре |
| `e2e\settings.spec.ts` | 3 | группы настроек, последовательная загрузка и расписание, блок ассоциации `.torrent` |
| `e2e\trackers.spec.ts` | 2 | вкладка «Трекеры» в деталях и редактор трекеров |
| `e2e\diagnostics.spec.ts` | 2 | окно диагностики кнопкой в сайдбаре и по `Ctrl+D` |
| `e2e\sort.spec.ts` | 1 | сортировка по двум ключам и сохранение в `localStorage` |

Общие шаги и селекторы — в `e2e\helpers.ts` (`openApp`, `listRows`, `selectFilter`, `isVisible`). На чём держится стабильность набора: элементы ищутся по роли или тексту, а не по CSS-классам; пауз `waitForTimeout` нет — состояние ждётся через `expect(...)`; каждый тест начинается с `openApp(page)`, потому что состояние демо-АПИ не переживает перезагрузку страницы.

Чего E2E не проверяют: нативную оболочку (окно, трей, единственный экземпляр, установщик, файловая ассоциация, автообновление) и реальную сеть. Это закрыто Rust-тестами и ручной проверкой, а в CI E2E не запускаются — там нет ни Chrome, ни шага прогона (раздел 8).

## 7. Подпись обновлений

Автообновление Tauri 2 устроено так: приватным ключом подписывается пакет обновления (рядом с установщиком появляется `.sig`), публичный ключ зашит в приложение (`plugins.updater.pubkey` в `src-tauri\tauri.conf.json`) и проверяет подпись на стороне клиента. Без пары ключей механизм включить нельзя, а подделать проверку нельзя даже при компрометации сервера. Это **не** Authenticode-подпись кода: сертификат для SmartScreen и ключ апдейтера — разные вещи (подробно — в [UPDATE.md](UPDATE.md), раздел 2).

Генерация пары:

```powershell
.\scripts\gen-updater-key.ps1
```

Скрипт вызывает официальный `tauri signer generate`, спрашивает пароль скрыто, создаёт приватный ключ с публичным `.pub` рядом и печатает **только публичный** ключ — его и нужно вставить в `tauri.conf.json`. Приватный ключ скрипт не читает и не печатает.

| Параметр | Что делает |
| --- | --- |
| `-KeyPath <путь>` | куда положить ключ; относительный путь считается от корня репозитория |
| `-Password <пароль>` | неинтерактивный запуск: пароль не спрашивается |
| `-Force` | перезаписать существующую пару (старым ключом подписаны прошлые обновления — менять его можно только вместе со сменой `pubkey` и публикацией новой версии) |
| `-Help` | справка и выход с кодом 0 |

По умолчанию ключи лежат в `.tauri-keys\torrenta.key` и `.tauri-keys\torrenta.key.pub` в корне репозитория — каталог уже в `.gitignore`. Tauri CLI по умолчанию кладёт ключи в `%USERPROFILE%\.tauri\<имя>.key`; если нужен такой путь, передайте его явно: `-KeyPath "$HOME\.tauri\torrenta.key"`. Приватный ключ в репозиторий не попадает ни при каком варианте — это проверяет сам скрипт.

Дальше: публичный ключ — в `plugins.updater.pubkey`, приватный — в секреты CI (`TAURI_SIGNING_PRIVATE_KEY`, при пароле ещё `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`) или в переменные окружения локальной сборки, артефакты обновления включаются в `tauri.conf.json`. Полный цикл — шаги 2–6 в [UPDATE.md](UPDATE.md).

Пока ключей нет, всё работает: `check_update` возвращает `configured: false` и пояснение «обновления не настроены: нет ключа подписи» — это не ошибка сети, а честное состояние. В поставке 1.7.0 ключа нет, поэтому автообновление выключено.

## 8. Непрерывная интеграция

Файл [`.github/workflows/build.yml`](../.github/workflows/build.yml) запускается на `windows-latest` в трёх случаях: push в `main`, pull request в `main` и ручной запуск через `workflow_dispatch` (Actions → workflow «Сборка» → Run workflow). В нём два задания, и набор шагов у них разный.

**Задание «Проверки (fmt, clippy, тесты)»** — только `pull_request` и `workflow_dispatch`. Это быстрый гейт качества:

1. checkout, `actions/setup-node@v4` (Node.js 20, кэш npm);
2. `dtolnay/rust-toolchain@stable` с компонентами `rustfmt` и `clippy`, затем `Swatinem/rust-cache@v2`;
3. печать версий инструментов (`rustc -V`, `cargo -V`, `node -v`, `npm -v`) — по логу видно, чем собирали;
4. `npm ci`;
5. `npm run build` — проверка типов и сборка интерфейса в `dist\` (без неё не соберётся Rust: `tauri::generate_context!()` требует `frontendDist`);
6. `cargo fmt --all --check` — форматирование всего workspace;
7. `cargo clippy --workspace --all-targets -- -D warnings` — предупреждения как ошибки;
8. `cargo test --workspace` — весь Rust-набор.

**Задание «Сборка (Windows x64)»** — на всех трёх триггерах, включая push в `main`:

1. то же окружение плюс `rustup show`;
2. `.\build-windows.ps1 -Help` — попутно проверяется, что релизный скрипт жив и печатает справку;
3. `npm ci`;
4. `npm run check:contrast` — контраст палитры;
5. `npm run build` — обязателен до компиляции Rust: результат попадает в `dist\` и встраивается в бинарник;
6. `cargo test --workspace`;
7. `cargo build --release`;
8. выгрузка `target\release\torrenta.exe` артефактом `torrenta-windows-x64` (хранится 14 дней; сборка падает, если файла нет).

Что CI **не** делает:

- не собирает установщики NSIS/MSI — это локальная работа `build-windows.ps1`;
- не подписывает код и не публикует обновления: сертификата Authenticode в проекте нет, ключей апдейтера в CI тоже (раздел 7);
- не запускает E2E: в раннере нет Google Chrome, а прогону нужны собранный `dist\` и `vite preview` на 4173 (раздел 6) — набор гоняется локально;
- не реагирует на теги: в триггерах фильтр `branches: [main]`, push тега вида `v1.7.0` в него не попадает. Релиз собирается локально и публикуется вручную, отдельного release-workflow нет.

Практическое следствие: на push в `main` идёт только задание «Сборка», а `fmt` и `clippy` есть лишь в PR и при ручном запуске. Поэтому оба гейта надо прогнать локально до отправки изменений (раздел 9).

## 9. Перед отправкой изменений

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npm run build
npm run check:contrast
npm run test:e2e
```

Порядок важен: `npm run build` должен пройти до `cargo test`, иначе `tauri::generate_context!()` не найдёт `dist` и тесты приложения не соберутся. `npm run test:e2e` идёт последним и требует свежего `dist\` — иначе `vite preview` отдаст старый бандл (раздел 6).

Ориентиры на 1.7.0, чтобы понимать, что «зелёный» вывод — норма, а не совпадение:

- `cargo test --workspace` → **428 passed, 0 failed, 1 ignored**: 222 теста в библиотеке `torrent-core` (221 passed и 1 ignored — тот самый UPnP-тест, см. 5.16), 45 интеграционных в 8 бинарях каталога `crates\torrent-core\tests` (10 + 2 + 2 + 4 + 5 + 8 + 10 + 4) и 162 в `torrenta`. Итого 429 тестовых функций;
- `npm run check:contrast` → **68 из 68** (34 пары в каждой из двух тем);
- `npx playwright test` → **12 passed, 0 skipped** примерно за 9 секунд.

Если тестов стало меньше — часть отфильтрована или не собралась; если больше — вы добавили свои, тогда обновите цифры в этом разделе.

А если меняли `build-windows.ps1` — убедитесь, что он печатает справку:

```powershell
.\build-windows.ps1 -Help
```

## 10. См. также

- [FEATURE-1.7.md](FEATURE-1.7.md) — что нового в 1.7.0: расписание лимитов, последовательная загрузка, множественная сортировка, ассоциация `.torrent`, заготовка автообновления.
- [UPDATE.md](UPDATE.md) — автообновление: ключи подписи, эндпоинт, публикация релиза и проверка.
- [README.md](../README.md) — обзор проекта, возможности, ограничения.
- [ARCHITECTURE.md](ARCHITECTURE.md) — устройство слоёв, задачи tokio, принятые решения.
- [CORE_API.md](CORE_API.md) — публичный API ядра `torrent-core`.
