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

Опционально:

- **WebView2 Runtime** нужен и для сборки (`tauri build` проверяет его наличие), и для запуска. Установщик Torrenta умеет доставлять его сам (режим `embedBootstrapper`, тихая установка).

Все зависимости ставятся бесплатно и без аккаунтов.

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
| Установщик NSIS | `target\release\bundle\nsis\Torrenta_1.3.1_x64-setup.exe` |
| Установщик MSI | `target\release\bundle\msi\Torrenta_1.3.1_x64_*.msi` |

Порядок и хуки: перед сборкой Rust Tauri сам запускает `beforeBuildCommand` из `src-tauri\tauri.conf.json` — то есть `npm run build`; результат попадает в `dist` (`frontendDist`) и встраивается в бинарник. Поэтому отдельный `npm run build` перед `tauri build` не обязателен: он нужен только для проверки типов и размера бандла без сборки приложения.

Для быстрой итерации по интерфейсу без пересборки Rust:

```powershell
npm run dev            # Vite на http://localhost:5173 (порт фиксирован: strictPort)
npm run tauri:dev      # окно Tauri с горячей перезагрузкой
```

## 4. Куда уходит время и место

- Первая сборка самая долгая: компилируется весь граф зависимостей (Tauri, tokio, rustls). Дальше работает инкрементальная компиляция.
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
3. Проверить, что `Torrenta_1.3.1_x64-setup.exe` не запущен в этот момент.

### 5.14 `error: package torrent-core v1.3.1 ... requires rustc 1.77`

**Причина:** устаревший Rust.

**Решение:** `rustup update stable`, затем `rustc -V`.

## 6. Непрерывная интеграция

Файл [`.github/workflows/build.yml`](../.github/workflows/build.yml) собирает проект на `windows-latest` при пуше в `main` и в pull request:

1. checkout, `actions/setup-node@v4` с Node.js 20 и кэшем npm;
2. `dtolnay/rust-toolchain@stable` (MSVC) и `Swatinem/rust-cache@v2` с кэшем Cargo;
3. `npm ci` → `npm run check:contrast` → `npm run build`;
4. `cargo test --workspace`;
5. `cargo build --release`;
6. выгрузка `target\release\torrenta.exe` как артефакта сборки.

Запуск установщика и подпись кода в CI не выполняются: подписи сертификатом в проекте нет, установщики собираются локально через `build-windows.ps1`.

## 7. Перед отправкой изменений

```powershell
cargo fmt --all
cargo test --workspace
npm run check:contrast
npm run build
.\build-windows.ps1 -Help      # скрипт сборки должен печатать справку без ошибок
```

## 8. См. также

- [README.md](../README.md) — обзор проекта, возможности, ограничения.
- [ARCHITECTURE.md](ARCHITECTURE.md) — устройство слоёв, задачи tokio, принятые решения.
- [CORE_API.md](CORE_API.md) — публичный API ядра `torrent-core`.
