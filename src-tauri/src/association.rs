//! Ассоциация `.torrent`-файлов с Torrenta (самолечение после обновлений).
//!
//! Windows хранит привязку расширения к программе в нескольких слоях реестра,
//! и старые сборки Torrenta оставляли там путь к своему exe. После обновления
//! приложение переезжает в другой каталог, а привязка продолжает указывать на
//! прежний (уже несуществующий) файл — в результате двойной клик по `.torrent`
//! открывает десяток отдельных окон без диалога добавления торрента.
//!
//! Отдельная беда — слой `UserChoice*` с пустым значением `ProgId`: он
//! перекрывает здоровые нижние слои, и Windows не открывает `.torrent` вообще —
//! ни запуска, ни диалога «Открыть с помощью». Такой слой можно только удалить:
//! `ProgId` в нём защищён хешем, который умеет считать лишь сама оболочка.
//!
//! Модуль решает обе проблемы на уровне пользователя (`HKCU`), не требуя прав
//! администратора:
//!
//! * читает фактический обработчик расширения в том порядке, в котором его
//!   видит оболочка: `UserChoiceLatest` → `UserChoice` →
//!   `HKCU\Software\Classes\.torrent`;
//! * принимает решение чистой функцией [`decide`] — её легко покрыть тестами,
//!   потому что она не трогает реестр;
//! * перезаписывает команду открытия на текущий exe ([`ensure`] при старте,
//!   [`repair`] по кнопке в настройках), а заведомо нерабочий слой
//!   `UserChoice*` удаляет;
//! * сверяет результат с тем, что реально ответит оболочка
//!   ([`resolve_executable`]).
//!
//! Чужой обработчик, который жив и работает (например, qBittorrent), намеренно
//! не трогается: перехват чужого выбора — это то, за что приложения справедливо
//! ругают. Если `.torrent` открывает другая живая программа, [`ensure`]
//! возвращает состояние с `is_owner == false` и пояснением в `message`, а
//! пользователь может поменять программу штатно через «Параметры Windows →
//! Приложения по умолчанию».
//!
//! На не-Windows платформах все функции — заглушки: они возвращают состояние
//! «ассоциация недоступна» и ничего не пишут.

use std::path::{Path, PathBuf};

/// ProgId приложения для торрент-файлов.
pub const TORRENT_PROGID: &str = "Torrenta.torrent";
/// ProgId приложения для magnet-ссылок.
pub const MAGNET_PROGID: &str = "Torrenta.magnet";
/// ProgId, под которым Windows 10/11 видит приложение в «Открыть с помощью».
pub const APPLICATIONS_PROGID: &str = "Applications\\torrenta.exe";
/// Общая заглушка Windows для расширения без постоянного обработчика.
pub const AUTO_FILE_PROGID: &str = "torrent_auto_file";
/// Расширение файла, которым управляет Torrenta.
pub const TORRENT_EXTENSION: &str = ".torrent";
/// Описание типа файла в диалоге «Открыть с помощью».
pub const TORRENT_DESCRIPTION: &str = "Торрент-файл Torrenta";
/// Описание magnet-ссылок.
pub const MAGNET_DESCRIPTION: &str = "Ссылка magnet Torrenta";
/// Имя приложения для проводника.
pub const APP_NAME: &str = "Torrenta";

/// Ключ `UserChoiceLatest` (Windows 11) с ProgId выбранного обработчика.
pub const USER_CHOICE_LATEST_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.torrent\UserChoiceLatest";
/// Ключ `UserChoice` (Windows 10 и 11) с ProgId выбранного обработчика.
pub const USER_CHOICE_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.torrent\UserChoice";
/// Имя значения с ProgId в ключах `UserChoice*`.
pub const USER_CHOICE_VALUE: &str = "ProgId";
/// Ключ расширения `.torrent` в ветке классов пользователя.
pub const CLASSES_TORRENT_KEY: &str = r"Software\Classes\.torrent";
/// Ключ ProgId торрент-файлов.
pub const TORRENT_PROGID_KEY: &str = r"Software\Classes\Torrenta.torrent";
/// Значок ProgId торрент-файлов.
pub const TORRENT_ICON_KEY: &str = r"Software\Classes\Torrenta.torrent\DefaultIcon";
/// Команда открытия ProgId торрент-файлов.
pub const TORRENT_COMMAND_KEY: &str = r"Software\Classes\Torrenta.torrent\shell\open\command";
/// Ключ ProgId magnet-ссылок.
pub const MAGNET_PROGID_KEY: &str = r"Software\Classes\Torrenta.magnet";
/// Значок ProgId magnet-ссылок.
pub const MAGNET_ICON_KEY: &str = r"Software\Classes\Torrenta.magnet\DefaultIcon";
/// Команда открытия ProgId magnet-ссылок.
pub const MAGNET_COMMAND_KEY: &str = r"Software\Classes\Torrenta.magnet\shell\open\command";
/// Ключ приложения в списке «Открыть с помощью».
pub const APPLICATIONS_KEY: &str = r"Software\Classes\Applications\torrenta.exe";
/// Значок приложения в списке «Открыть с помощью».
pub const APPLICATIONS_ICON_KEY: &str = r"Software\Classes\Applications\torrenta.exe\DefaultIcon";
/// Команда открытия приложения (её и держит `ProgId` из `UserChoice`).
pub const APPLICATIONS_COMMAND_KEY: &str =
    r"Software\Classes\Applications\torrenta.exe\shell\open\command";
/// Список типов файлов приложения.
pub const APPLICATIONS_TYPES_KEY: &str =
    r"Software\Classes\Applications\torrenta.exe\SupportedTypes";
/// Команда открытия заглушки `torrent_auto_file` (её пишут старые сборки).
pub const AUTO_FILE_COMMAND_KEY: &str = r"Software\Classes\torrent_auto_file\shell\open\command";

/// Аргумент команды открытия: путь к открываемому файлу.
const COMMAND_ARG: &str = "%1";
/// Часть имени exe, по которой узнаётся «наш» обработчик.
const EXE_STEM: &str = "torrenta";
/// Хвост имени исполняемого файла.
const EXE_SUFFIX: &str = ".exe";
/// Префикс «вербатимного» пути, который отдаёт `std::env::current_exe()`.
const VERBATIM_PREFIX: &str = r"\\?\";
/// Префикс вербатимного пути к сетевой шаре.
const VERBATIM_UNC_PREFIX: &str = r"\\?\UNC\";
/// Размер одного символа UTF-16 в байтах.
const WIDE_CHAR_BYTES: usize = std::mem::size_of::<u16>();
/// Текст «ассоциация поддерживается только в Windows».
/// Причина недоступности ассоциаций: поддержка есть только в Windows.
pub const UNSUPPORTED_PLATFORM: &str = "ассоциация файлов поддерживается только в Windows";
/// Подсказка, если `.torrent` удерживает живая чужая программа.
const FOREIGN_HINT: &str =
    "выбрать Torrenta можно в «Параметрах Windows → Приложения по умолчанию»";

/// Откуда взят текущий ProgId обработчика `.torrent`-файлов.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HandlerSource {
    /// Привязки нет ни на одном слое.
    #[default]
    Missing,
    /// ProgId взят из `HKCU\Software\Classes\.torrent`.
    Classes,
    /// ProgId выбран в `UserChoiceLatest` (Windows 11).
    UserChoiceLatest,
    /// ProgId выбран в `UserChoice` (Windows 10 и 11).
    UserChoice,
    /// Ключ `UserChoice*` есть, но ProgId в нём пустой — привязка сломана.
    EmptyChoice(UserChoiceKey),
}

impl HandlerSource {
    /// Сломанный слой `UserChoice*`, который надо удалить целиком.
    pub fn broken_choice(self) -> Option<UserChoiceKey> {
        match self {
            Self::EmptyChoice(key) => Some(key),
            _ => None,
        }
    }

    /// Слой `UserChoice*`, из которого прочитан ProgId.
    pub fn choice_key(self) -> Option<UserChoiceKey> {
        match self {
            Self::UserChoiceLatest => Some(UserChoiceKey::Latest),
            Self::UserChoice => Some(UserChoiceKey::Current),
            Self::EmptyChoice(key) => Some(key),
            Self::Missing | Self::Classes => None,
        }
    }
}

/// Ключ `UserChoice*` в реестре.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserChoiceKey {
    /// `UserChoiceLatest` (Windows 11).
    Latest,
    /// `UserChoice` (Windows 10 и 11).
    Current,
}

impl UserChoiceKey {
    /// Путь ключа в реестре.
    pub fn reg_path(self) -> &'static str {
        match self {
            Self::Latest => USER_CHOICE_LATEST_KEY,
            Self::Current => USER_CHOICE_KEY,
        }
    }
}

/// Текущий обработчик `.torrent`-файлов: то, что реально вернул бы проводник.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HandlerView {
    /// ProgId обработчика (`None` — ProgId не задан).
    pub progid: Option<String>,
    /// Команда открытия из `shell\open\command` найденного ProgId.
    pub command: Option<String>,
    /// Слой реестра, из которого взят ProgId.
    pub source: HandlerSource,
}

impl HandlerView {
    /// Обработчик, найденный в конкретном слое реестра.
    pub fn with_source(
        progid: Option<String>,
        command: Option<String>,
        source: HandlerSource,
    ) -> Self {
        Self {
            progid,
            command,
            source,
        }
    }

    /// Сломанный слой `UserChoice*` с пустым ProgId.
    pub fn broken(key: UserChoiceKey) -> Self {
        Self {
            source: HandlerSource::EmptyChoice(key),
            ..Self::default()
        }
    }
}

/// Решение по текущему обработчику `.torrent`-файлов.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Обработчик — чужое живое приложение: не трогаем его.
    Keep,
    /// Обработчика нет (или он сломан): регистрируем Torrenta.
    Claim,
    /// Обработчик наш, но команда ведёт на старый/несуществующий exe: правим путь.
    RepairStalePath,
}

/// Состояние ассоциации `.torrent`-файлов после проверки или починки.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssociationState {
    /// ProgId, который сейчас обрабатывает `.torrent`.
    pub handler: Option<String>,
    /// Команда открытия этого обработчика.
    pub command: Option<String>,
    /// Открывает ли `.torrent` именно текущий exe Torrenta.
    pub is_owner: bool,
    /// Пришлось ли (пере)записать ассоциацию в этом вызове.
    pub repaired: bool,
    /// Пояснение для интерфейса: почему привязка не наша или что не удалось.
    pub message: Option<String>,
}

/// Managed-состояние Tauri: последний результат проверки или починки привязки.
///
/// Команды `association_status`/`repair_association` обновляют его, а `setup`
/// кладёт туда результат проверки при старте.
pub type AssociationSlot = std::sync::Mutex<AssociationState>;

impl AssociationState {
    /// Состояние «ассоциация недоступна» с причиной для интерфейса.
    pub fn unavailable(reason: String) -> Self {
        Self {
            message: Some(reason),
            ..Self::default()
        }
    }
}

/// Команда открытия файла текущим exe: `"<exe>" "%1"`.
///
/// Кавычки обязательны: путь может содержать пробелы (`C:\Program Files\...`),
/// иначе Windows запустит только первый сегмент пути.
pub fn command_for(exe: &Path) -> String {
    format!("\"{}\" \"{COMMAND_ARG}\"", exe.display())
}

/// Значение `DefaultIcon`: `"<exe>",0`.
pub fn icon_for(exe: &Path) -> String {
    format!("\"{}\",0", exe.display())
}

/// Является ли ProgId «нашим» (включая общую заглушку Windows).
///
/// Сравнение регистронезависимое: реестр Windows не различает регистр имён
/// ключей, и старые сборки могли записать `torrenta.torrent`.
pub fn is_our_progid(progid: &str) -> bool {
    [
        TORRENT_PROGID,
        MAGNET_PROGID,
        APPLICATIONS_PROGID,
        AUTO_FILE_PROGID,
    ]
    .iter()
    .any(|known| progid.trim().eq_ignore_ascii_case(known))
}

/// Похож ли путь на сборку Torrenta (по имени исполняемого файла).
///
/// Используется только для старых обработчиков: если команда ведёт на
/// `torrenta.exe` из другого каталога, это наша же прошлая версия, и её путь
/// надо исправить, а не оставлять как «чужую программу».
pub fn looks_like_torrenta(exe: &Path) -> bool {
    exe.file_stem()
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|stem| stem.contains(EXE_STEM))
}

/// Вытаскивает путь к исполняемому файлу из команды открытия.
///
/// Команда в реестре выглядит по-разному: `"C:\...\torrenta.exe" "%1"`,
/// `C:\...\torrenta.exe %1`, `C:\Program Files\...\torrenta.exe --open "%1"`.
/// Возвращаются только путь без кавычек и без аргументов.
pub fn extract_exe(command: &str) -> Option<PathBuf> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return None;
    }

    let raw = if let Some(rest) = trimmed.strip_prefix('"') {
        // Путь в кавычках: до закрывающей кавычки (незакрытая — до конца строки).
        match rest.find('"') {
            Some(end) => &rest[..end],
            None => rest,
        }
    } else if has_drive_prefix(trimmed) || trimmed.starts_with(r"\\") {
        // Путь без кавычек: Windows разрешает пробелы, поэтому режем по `.exe`.
        until_exe(trimmed)
    } else {
        // Первое слово команды: `%ProgramFiles%\...`, `torrenta.exe` и т. п.
        trimmed.split_whitespace().next().unwrap_or_default()
    };

    let cleaned = raw.trim().trim_matches('"').trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(PathBuf::from(cleaned))
    }
}

/// Указывают ли два пути на один и тот же файл.
///
/// Пути Windows регистронезависимы, могут содержать прямые слэши и
/// вербатимный префикс `\\?\`, который отдаёт `std::env::current_exe()`.
pub fn is_same_path(left: &Path, right: &Path) -> bool {
    match (normalize_path(left), normalize_path(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Указывает ли команда открытия ровно на этот exe.
pub fn is_our_command(command: &str, exe: &Path) -> bool {
    match extract_exe(command) {
        Some(found) => is_same_path(&found, exe),
        None => false,
    }
}

/// Открывает ли `.torrent` именно этот exe (по данным реестра).
pub fn handler_is_ours(handler: &HandlerView, exe: &Path) -> bool {
    handler
        .command
        .as_deref()
        .is_some_and(|command| is_our_command(command, exe))
}

/// Главное решение модуля: что делать с текущим обработчиком.
///
/// Функция чистая (не читает реестр и не пишет в него), поэтому её поведение
/// целиком проверяется тестами.
pub fn decide(current: &HandlerView, exe: &Path) -> Decision {
    // Сломанный слой `UserChoice*` перекрывает всё: Windows с пустым ProgId не
    // открывает ничего вообще.
    if current.source.broken_choice().is_some() {
        return Decision::Claim;
    }

    let command = non_empty(current.command.as_deref());
    let Some(progid) = non_empty(current.progid.as_deref()) else {
        // Привязки нет вовсе — регистрируем себя.
        return Decision::Claim;
    };

    if !is_our_progid(progid) {
        // Чужой ProgId: перехватываем только когда открывать им нечего.
        return decide_foreign(command, exe);
    }

    if progid.eq_ignore_ascii_case(AUTO_FILE_PROGID) {
        // `torrent_auto_file` — общая заглушка расширения. Через «Открыть с
        // помощью» её мог заполнить и чужой живой обработчик: такой выбор
        // пользователя сохраняем, а старую запись Torrenta — правим.
        if let Some(command) = command {
            if let Some(cmd_exe) = extract_exe(command) {
                let foreign_alive = !is_our_command(command, exe)
                    && !looks_like_torrenta(&cmd_exe)
                    && cmd_exe.is_file();
                if foreign_alive {
                    return Decision::Keep;
                }
            }
        }
        return Decision::RepairStalePath;
    }

    // ProgId наш: всё хорошо только если команда уже указывает на текущий exe.
    match command {
        Some(command) if is_our_command(command, exe) => Decision::Keep,
        _ => Decision::RepairStalePath,
    }
}

/// Решение для ProgId, который принадлежит не нам.
fn decide_foreign(command: Option<&str>, exe: &Path) -> Decision {
    let Some(command) = command else {
        // Чужой ProgId без команды ничего не открывает — забираем расширение.
        return Decision::Claim;
    };
    let Some(cmd_exe) = extract_exe(command) else {
        return Decision::Claim;
    };

    if is_our_command(command, exe) {
        // Имя ProgId чужое, но открывается уже этот exe: достаточно поправить ключи.
        return Decision::RepairStalePath;
    }
    if looks_like_torrenta(&cmd_exe) {
        // Старая сборка Torrenta под чужим ProgId: правим путь, а не заводим второй.
        return Decision::RepairStalePath;
    }
    if cmd_exe.is_file() {
        // Чужая живая программа: не перехватываем, даже если это qBittorrent.
        return Decision::Keep;
    }
    // Чужой ProgId ведёт в пустоту (программу удалили) — расширение свободно.
    Decision::Claim
}

/// Проверяет ассоциацию и при необходимости починит её.
///
/// Вызывается при старте приложения: после обновления достаточно одного запуска,
/// чтобы `.torrent` снова открывался установленной версией.
pub fn ensure(exe: &Path) -> AssociationState {
    let current = match read_handler() {
        Ok(current) => current,
        Err(err) => {
            eprintln!("[torrenta] не удалось прочитать ассоциацию .torrent: {err}");
            return AssociationState::unavailable(err);
        }
    };

    match decide(&current, exe) {
        Decision::Keep => state_for(&current, exe, false),
        Decision::Claim | Decision::RepairStalePath => match heal(&current, exe) {
            Ok(()) => reread(exe),
            Err(err) => failed_state(&current, exe, err),
        },
    }
}

/// Только читает текущее состояние ассоциации, ничего не меняя.
pub fn status(exe: &Path) -> AssociationState {
    match read_handler() {
        Ok(current) => state_for(&current, exe, false),
        Err(err) => {
            eprintln!("[torrenta] не удалось прочитать ассоциацию .torrent: {err}");
            AssociationState::unavailable(err)
        }
    }
}

/// Принудительно (пере)регистрирует ассоциацию с этим exe.
///
/// Используется кнопкой «Связать файлы .torrent» в настройках: делает то же,
/// что и [`ensure`], но по явному требованию пользователя.
pub fn repair(exe: &Path) -> AssociationState {
    let current = match read_handler() {
        Ok(current) => current,
        Err(err) => {
            eprintln!("[torrenta] не удалось прочитать ассоциацию .torrent: {err}");
            return AssociationState::unavailable(err);
        }
    };
    match heal(&current, exe) {
        Ok(()) => reread(exe),
        Err(err) => failed_state(&current, exe, err),
    }
}

/// Путь к программе, которой Windows откроет `.torrent` (по ответу оболочки).
///
/// `Ok(None)` — оболочка не может разрешить привязку (например, в
/// `UserChoice*` пустой ProgId): тогда двойной клик не запустит ничего.
pub fn resolve_executable() -> Result<Option<PathBuf>, String> {
    shell_resolve()
}

/// Полный путь к текущему исполняемому файлу — цель ассоциации.
///
/// Именно `current_exe`, а не каталог данных: в портативном режиме `.torrent`
/// должен открывать тот же бинарник, который сейчас работает.
pub fn current_exe() -> Option<PathBuf> {
    std::env::current_exe().ok()
}

/// Приводит ассоциацию в рабочее состояние: удаляет нерабочий слой
/// `UserChoice*` (если он есть) и записывает ключи Torrenta.
fn heal(current: &HandlerView, exe: &Path) -> Result<(), String> {
    if let Some(key) = broken_choice_to_delete(current, exe) {
        // Записать в `UserChoice*` свой ProgId нельзя: оболочка проверяет хеш,
        // который считает только сама. Удаление возвращает работу нижним слоям.
        delete_key(key.reg_path())?;
    }
    register(exe)
}

/// Слой `UserChoice*`, который нужно удалить, чтобы привязка заработала.
///
/// Удаляется только заведомо нерабочий выбор: пустой ProgId (Windows в этом
/// случае не открывает ничего и не показывает диалог) или чужой обработчик,
/// чья программа больше не существует. Живой чужой обработчик не трогаем
/// никогда, а свой слой чинится перезаписью команды.
fn broken_choice_to_delete(current: &HandlerView, exe: &Path) -> Option<UserChoiceKey> {
    if let Some(key) = current.source.broken_choice() {
        return Some(key);
    }
    let key = current.source.choice_key()?;
    if matches!(decide(current, exe), Decision::Keep) {
        // Живой чужой выбор — не наше дело.
        return None;
    }
    let progid = current.progid.as_deref()?;
    if is_our_progid(progid) {
        // Свой ProgId (и заглушка `torrent_auto_file`) чинится на месте.
        return None;
    }
    Some(key)
}

/// Перечитывает обработчик после записи: статус должен совпадать с тем, что
/// видит оболочка, а не с тем, что мы туда положили.
fn reread(exe: &Path) -> AssociationState {
    match read_handler() {
        Ok(current) => state_for(&current, exe, true),
        Err(err) => {
            // Запись прошла, а чтение — нет (например, политика запрета):
            // показываем ожидаемое состояние, но честно сообщаем об ошибке.
            eprintln!("[torrenta] не удалось перечитать ассоциацию .torrent: {err}");
            AssociationState {
                handler: Some(TORRENT_PROGID.to_string()),
                command: Some(command_for(exe)),
                is_owner: true,
                repaired: true,
                message: Some(err),
            }
        }
    }
}

/// Состояние ассоциации по уже прочитанному обработчику.
fn state_for(current: &HandlerView, exe: &Path, repaired: bool) -> AssociationState {
    let resolved = match shell_resolve() {
        Ok(resolved) => resolved,
        Err(err) => {
            eprintln!("[torrenta] не удалось спросить оболочку об ассоциации .torrent: {err}");
            None
        }
    };
    // Владелец — если так считает оболочка или, как запасной вариант, реестр:
    // ответ оболочки может отставать сразу после записи ключей.
    let shell_ours = resolved
        .as_deref()
        .is_some_and(|path| is_same_path(path, exe));
    let registry_ours = handler_is_ours(current, exe);
    let is_owner = shell_ours || registry_ours;

    AssociationState {
        handler: current.progid.clone(),
        command: current.command.clone(),
        is_owner,
        repaired,
        message: hint_for(current, exe, is_owner),
    }
}

/// Состояние неудачной починки: что было и что не получилось.
fn failed_state(current: &HandlerView, exe: &Path, err: String) -> AssociationState {
    eprintln!("[torrenta] не удалось исправить ассоциацию .torrent: {err}");
    AssociationState {
        handler: current.progid.clone(),
        command: current.command.clone(),
        is_owner: handler_is_ours(current, exe),
        repaired: false,
        message: Some(err),
    }
}

/// Пояснение для интерфейса, если `.torrent` открывает не Torrenta.
fn hint_for(current: &HandlerView, exe: &Path, is_owner: bool) -> Option<String> {
    if is_owner {
        return None;
    }
    if let Some(key) = current.source.broken_choice() {
        return Some(format!(
            "привязка сломана: в «{}» пустой ProgId, файлы .torrent не открываются",
            key.reg_path()
        ));
    }
    foreign_app(current, exe)
        .map(|name| format!("файлы .torrent открывает другая программа ({name}), {FOREIGN_HINT}"))
}

/// Имя живой чужой программы, которая сейчас открывает `.torrent`.
fn foreign_app(current: &HandlerView, exe: &Path) -> Option<String> {
    let command = current.command.as_deref()?;
    if is_our_command(command, exe) {
        return None;
    }
    let cmd_exe = extract_exe(command)?;
    if looks_like_torrenta(&cmd_exe) || !cmd_exe.is_file() {
        return None;
    }
    cmd_exe
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
}

/// Строковое значение, если оно непустое (пустые значения реестра — норма).
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// Начинается ли строка с буквы диска (`C:`).
fn has_drive_prefix(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Отрезает хвост команды после `.exe` (аргументы, записанные без кавычек).
fn until_exe(text: &str) -> &str {
    // Понижение регистра ASCII не меняет длину UTF-8, поэтому найденный индекс
    // годится и для исходной строки с не-ASCII символами (кириллица в пути).
    let lowered = text.to_ascii_lowercase();
    match lowered.find(EXE_SUFFIX) {
        Some(index) => &text[..index + EXE_SUFFIX.len()],
        None => text.split_whitespace().next().unwrap_or(text),
    }
}

/// Приводит путь к виду, пригодному для сравнения путей Windows.
///
/// Убирает кавычки, вербатимный префикс `\\?\`, прямые слэши и хвостовой
/// разделитель, понижает регистр (пути Windows регистронезависимы).
fn normalize_path(path: &Path) -> Option<String> {
    let raw = path.to_string_lossy();
    let trimmed = raw.trim().trim_matches('"').trim();
    if trimmed.is_empty() {
        return None;
    }

    let stripped = if let Some(rest) = trimmed.strip_prefix(VERBATIM_UNC_PREFIX) {
        // `\\?\UNC\server\share` и `\\server\share` — один и тот же путь.
        format!(r"\\{rest}")
    } else if let Some(rest) = trimmed.strip_prefix(VERBATIM_PREFIX) {
        rest.to_string()
    } else {
        trimmed.to_string()
    };

    let unified = stripped.replace('/', r"\");
    let without_tail = unified.trim_end_matches('\\');
    if without_tail.is_empty() {
        return None;
    }
    Some(without_tail.to_lowercase())
}

/// Читает текущий обработчик `.torrent`-файлов из реестра.
#[cfg(windows)]
fn read_handler() -> Result<HandlerView, String> {
    // Порядок важен: оболочка Windows 11 смотрит сначала `UserChoiceLatest`,
    // затем `UserChoice`, и только потом — `HKCU\Software\Classes\.torrent`.
    for (key, source) in [
        (USER_CHOICE_LATEST_KEY, HandlerSource::UserChoiceLatest),
        (USER_CHOICE_KEY, HandlerSource::UserChoice),
    ] {
        if !key_exists(key)? {
            continue;
        }
        let progid = read_value(HKEY_CURRENT_USER, key, Some(USER_CHOICE_VALUE))?;
        let Some(progid) = non_empty(progid.as_deref()) else {
            // Ключ есть, а ProgId пустой (там остался только `Hash`): Windows не
            // открывает ничего и даже не показывает диалог выбора. Слой
            // перекрывает здоровые нижние, поэтому сообщаем о поломке.
            let broken = match source {
                HandlerSource::UserChoiceLatest => UserChoiceKey::Latest,
                _ => UserChoiceKey::Current,
            };
            return Ok(HandlerView::broken(broken));
        };
        let command = read_command(progid)?;
        return Ok(HandlerView::with_source(
            Some(progid.to_string()),
            command,
            source,
        ));
    }

    // Последний слой: обычная ветка классов пользователя.
    let progid = read_value(HKEY_CURRENT_USER, CLASSES_TORRENT_KEY, None)?;
    let Some(progid) = non_empty(progid.as_deref()) else {
        return Ok(HandlerView::default());
    };
    let command = read_command(progid)?;
    Ok(HandlerView::with_source(
        Some(progid.to_string()),
        command,
        HandlerSource::Classes,
    ))
}

/// Читает команду открытия для ProgId: сначала `HKCU`, затем `HKLM`.
#[cfg(windows)]
fn read_command(progid: &str) -> Result<Option<String>, String> {
    let subkey = format!(r"Software\Classes\{progid}\shell\open\command");
    if let Some(command) = read_value(HKEY_CURRENT_USER, &subkey, None)? {
        return Ok(Some(command));
    }
    read_value(HKEY_LOCAL_MACHINE, &subkey, None)
}

/// Записывает все ключи, по которым Windows узнаёт Torrenta как обработчика.
///
/// Все значения пишутся в `HKCU` (права обычного пользователя). Ассоциацию
/// расширения `HKCU\Software\Classes\.torrent` тоже приходится задавать самим:
/// иначе на машине без ассоциации двойной клик открывает диалог выбора
/// программы вместо добавления торрента.
#[cfg(windows)]
fn register(exe: &Path) -> Result<(), String> {
    let command = command_for(exe);
    let icon = icon_for(exe);

    // Основной ProgId приложения: описание, значок и команда открытия.
    write_value(TORRENT_PROGID_KEY, TORRENT_DESCRIPTION)?;
    write_value(TORRENT_ICON_KEY, &icon)?;
    write_value(TORRENT_COMMAND_KEY, &command)?;

    // ProgId для magnet-ссылок: та же программа, отдельное описание.
    write_value(MAGNET_PROGID_KEY, MAGNET_DESCRIPTION)?;
    write_value(MAGNET_ICON_KEY, &icon)?;
    write_value(MAGNET_COMMAND_KEY, &command)?;

    // Запись `Applications\torrenta.exe` — именно её держит `ProgId` из
    // `UserChoice`, поэтому исправление пути здесь чинит привязку без правки
    // защищённого ключа `UserChoice` (там оболочка проверяет свой `Hash`).
    write_value(APPLICATIONS_KEY, APP_NAME)?;
    write_value(APPLICATIONS_ICON_KEY, &icon)?;
    write_value(APPLICATIONS_COMMAND_KEY, &command)?;
    write_value(APPLICATIONS_TYPES_KEY, "")?;
    write_value_named(APPLICATIONS_TYPES_KEY, TORRENT_EXTENSION, "")?;

    // Заглушка `torrent_auto_file`: её тоже пишут старые сборки и её же может
    // держать `ProgId` из `UserChoice` — правим путь и там.
    write_value(AUTO_FILE_COMMAND_KEY, &command)?;

    // Расширение → наш ProgId: без этого строка остаётся привязанной к
    // `torrent_auto_file` (её и оставляли старые сборки).
    write_value(CLASSES_TORRENT_KEY, TORRENT_PROGID)?;

    Ok(())
}

/// Существует ли ключ реестра.
#[cfg(windows)]
fn key_exists(key: &str) -> Result<bool, String> {
    let key_wide = to_wide(key);
    let mut opened: HKEY = ptr::null_mut();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            key_wide.as_ptr(),
            0,
            KEY_READ,
            &mut opened,
        )
    };
    if status == ERROR_SUCCESS {
        unsafe { RegCloseKey(opened) };
        return Ok(true);
    }
    if is_missing(status) {
        return Ok(false);
    }
    Err(registry_error("не удалось открыть", key, None, status))
}

/// Удаляет ключ реестра вместе со всеми подключaми.
#[cfg(windows)]
fn delete_key(key: &str) -> Result<(), String> {
    let key_wide = to_wide(key);
    let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key_wide.as_ptr()) };
    if status == ERROR_SUCCESS || is_missing(status) {
        Ok(())
    } else {
        Err(registry_error("не удалось удалить", key, None, status))
    }
}

/// Читает строковое значение реестра.
///
/// `Ok(None)` — ключа или значения нет (это норма: расширение может быть не
/// привязано), `Err` — реестр действительно недоступен (например, запрещён
/// политикой).
#[cfg(windows)]
fn read_value(root: HKEY, key: &str, name: Option<&str>) -> Result<Option<String>, String> {
    let key_wide = to_wide(key);
    let name_wide = name.map(to_wide);
    let name_ptr = match name_wide.as_ref() {
        Some(wide) => wide.as_ptr(),
        None => ptr::null(),
    };

    let mut bytes: u32 = 0;
    // Первый вызов только узнаёт размер: значение может быть длиннее буфера.
    let status = unsafe {
        RegGetValueW(
            root,
            key_wide.as_ptr(),
            name_ptr,
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut bytes,
        )
    };
    if is_missing(status) {
        return Ok(None);
    }
    if status != ERROR_SUCCESS && status != ERROR_MORE_DATA {
        return Err(registry_error("не удалось прочитать", key, name, status));
    }
    if bytes == 0 {
        return Ok(None);
    }

    // `bytes` — размер в байтах вместе с завершающим нулём.
    let mut buffer = vec![0u16; (bytes as usize).div_ceil(WIDE_CHAR_BYTES)];
    let mut size = bytes;
    let status = unsafe {
        RegGetValueW(
            root,
            key_wide.as_ptr(),
            name_ptr,
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if is_missing(status) {
        // Значение удалили между вызовами — считаем, что его нет.
        return Ok(None);
    }
    if status != ERROR_SUCCESS {
        return Err(registry_error("не удалось прочитать", key, name, status));
    }

    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    Ok(Some(String::from_utf16_lossy(&buffer[..end])))
}

/// Записывает значение по умолчанию для ключа, создавая ключи при необходимости.
#[cfg(windows)]
fn write_value(key: &str, value: &str) -> Result<(), String> {
    write_value_inner(key, None, value, "не удалось записать")
}

/// Записывает именованное значение, создавая ключи при необходимости.
#[cfg(windows)]
fn write_value_named(key: &str, name: &str, value: &str) -> Result<(), String> {
    write_value_inner(key, Some(name), value, "не удалось записать")
}

/// Запись значения `REG_SZ` в `HKCU` без `RegCreateKeyExW`: `RegSetKeyValueW`
/// создаёт недостающие подключи сам.
#[cfg(windows)]
fn write_value_inner(
    key: &str,
    name: Option<&str>,
    value: &str,
    action: &str,
) -> Result<(), String> {
    let key_wide = to_wide(key);
    let name_wide = name.map(to_wide);
    let name_ptr = match name_wide.as_ref() {
        Some(wide) => wide.as_ptr(),
        None => ptr::null(),
    };
    let data = to_wide(value);

    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key_wide.as_ptr(),
            name_ptr,
            REG_SZ,
            data.as_ptr().cast(),
            (data.len() * WIDE_CHAR_BYTES) as u32,
        )
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(registry_error(action, key, name, status))
    }
}

/// Спрашивает оболочку, какой программой откроется `.torrent`.
#[cfg(windows)]
fn shell_resolve() -> Result<Option<PathBuf>, String> {
    let extension = to_wide(TORRENT_EXTENSION);

    let mut size: u32 = 0;
    // Первый вызов — только размер буфера. `hr < 0` здесь означает «оболочка
    // привязку не разрешает»: это нормальный ответ, а не ошибка.
    let hr = unsafe {
        AssocQueryStringW(
            ASSOCF_NONE,
            ASSOCSTR_EXECUTABLE,
            extension.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            &mut size,
        )
    };
    if hr < 0 || size == 0 {
        return Ok(None);
    }

    let mut buffer = vec![0u16; size as usize];
    let mut written = size;
    let hr = unsafe {
        AssocQueryStringW(
            ASSOCF_NONE,
            ASSOCSTR_EXECUTABLE,
            extension.as_ptr(),
            ptr::null(),
            buffer.as_mut_ptr(),
            &mut written,
        )
    };
    if hr < 0 {
        return Ok(None);
    }

    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    let path = String::from_utf16_lossy(&buffer[..end]);
    let path = path.trim();
    if path.is_empty() {
        Ok(None)
    } else {
        Ok(Some(PathBuf::from(path)))
    }
}

/// Отсутствует ли ключ или значение (это не ошибка, а обычное состояние).
#[cfg(windows)]
fn is_missing(status: WIN32_ERROR) -> bool {
    status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND
}

/// Текст ошибки реестра с кодом Windows — по нему видно причину (например, 5 —
/// доступ запрещён политикой).
#[cfg(windows)]
fn registry_error(action: &str, key: &str, name: Option<&str>, status: WIN32_ERROR) -> String {
    match name {
        Some(name) => format!("{action} «{key}» → «{name}»: код {status}"),
        None => format!("{action} «{key}»: код {status}"),
    }
}

/// Строка Win32 (UTF-16 с завершающим нулём).
#[cfg(windows)]
fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Заглушка для не-Windows: ассоциация файлов там устроена иначе.
#[cfg(not(windows))]
fn read_handler() -> Result<HandlerView, String> {
    Err(UNSUPPORTED_PLATFORM.to_string())
}

/// Заглушка для не-Windows: писать нечего и некуда.
#[cfg(not(windows))]
fn register(_exe: &Path) -> Result<(), String> {
    Err(UNSUPPORTED_PLATFORM.to_string())
}

/// Заглушка для не-Windows: удалять нечего.
#[cfg(not(windows))]
fn delete_key(_key: &str) -> Result<(), String> {
    Err(UNSUPPORTED_PLATFORM.to_string())
}

/// Заглушка для не-Windows: оболочки Windows рядом нет.
#[cfg(not(windows))]
fn shell_resolve() -> Result<Option<PathBuf>, String> {
    Ok(None)
}

#[cfg(windows)]
use core::ptr;

#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR,
};
#[cfg(windows)]
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteTreeW, RegGetValueW, RegOpenKeyExW, RegSetKeyValueW, HKEY,
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ, RRF_RT_REG_SZ,
};
#[cfg(windows)]
use windows_sys::Win32::UI::Shell::{AssocQueryStringW, ASSOCF_NONE, ASSOCSTR_EXECUTABLE};

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Уникальный временный каталог под файлы-заглушки «живых» exe.
    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "torrenta-assoc-{tag}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("временный каталог создаётся");
        dir
    }

    /// Создаёт файл, чтобы [`decide`] увидел существующий путь к exe.
    fn touch(path: &Path) -> PathBuf {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("каталог exe создаётся");
        }
        fs::write(path, b"MZ").expect("файл exe пишется");
        path.to_path_buf()
    }

    /// Обработчик с заданными ProgId и командой (слой — «классы», если не задан).
    fn view(progid: Option<&str>, command: Option<String>) -> HandlerView {
        HandlerView::with_source(progid.map(str::to_string), command, HandlerSource::Classes)
    }

    /// Тот же обработчик, но найденный в слое `UserChoiceLatest`.
    fn in_latest(progid: Option<&str>, command: Option<String>) -> HandlerView {
        HandlerView::with_source(
            progid.map(str::to_string),
            command,
            HandlerSource::UserChoiceLatest,
        )
    }

    /// Обработчик «нашей» установки с командой на текущий exe.
    fn own_handler(exe: &Path) -> HandlerView {
        view(Some(APPLICATIONS_PROGID), Some(command_for(exe)))
    }

    #[test]
    fn our_command_matches_current_exe() {
        let exe = Path::new(r"C:\Tools\Torrenta\torrenta.exe");
        assert!(is_our_command(&command_for(exe), exe));
        // Команда без аргумента тоже указывает на наш exe.
        assert!(is_our_command(r#""C:\Tools\Torrenta\torrenta.exe""#, exe));
        assert!(!is_our_command(r#""C:\Other\app.exe" "%1""#, exe));
    }

    #[test]
    fn our_progid_with_live_command_is_kept() {
        let dir = unique_dir("keep");
        let exe = touch(&dir.join("app").join("torrenta.exe"));
        assert_eq!(decide(&own_handler(&exe), &exe), Decision::Keep);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn repeated_ensure_finds_nothing_to_fix() {
        // `ensure` идемпотентен: состояние, которое он сам записывает (наш ProgId
        // и команда на текущий exe), при повторной проверке даёт `Keep` — значит
        // следующие запуски приложения реестр уже не трогают.
        let dir = unique_dir("idempotent");
        let exe = touch(&dir.join("app").join("torrenta.exe"));
        let healed = view(Some(TORRENT_PROGID), Some(command_for(&exe)));
        assert_eq!(decide(&healed, &exe), Decision::Keep);
        assert_eq!(decide(&healed, &exe), Decision::Keep);
        // Тот же вывод и для слоя `UserChoiceLatest`, которым пользуется Win11.
        let latest = in_latest(Some(TORRENT_PROGID), Some(command_for(&exe)));
        assert_eq!(decide(&latest, &exe), Decision::Keep);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_torrenta_path_needs_repair() {
        // Именно этот случай был на живой машине: ProgId указывает на 1.3.1,
        // каталог которой уже удалён.
        let dir = unique_dir("stale");
        let stale = dir.join("old").join("torrenta.exe");
        let current = dir.join("new").join("torrenta.exe");
        let handler = view(Some(APPLICATIONS_PROGID), Some(command_for(&stale)));
        assert_eq!(decide(&handler, &current), Decision::RepairStalePath);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stale_our_choice_layer_is_repaired_in_place() {
        // Наш ProgId в `UserChoiceLatest`, но путь старый: слой не удаляем —
        // достаточно переписать команду `Applications\torrenta.exe`.
        let dir = unique_dir("stale-choice");
        let current = dir.join("new").join("torrenta.exe");
        let handler = in_latest(
            Some(APPLICATIONS_PROGID),
            Some(command_for(&dir.join("old").join("torrenta.exe"))),
        );
        assert_eq!(decide(&handler, &current), Decision::RepairStalePath);
        assert_eq!(broken_choice_to_delete(&handler, &current), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn live_foreign_handler_is_kept() {
        // У пользователя выбран живой qBittorrent — перехватывать нельзя.
        let dir = unique_dir("foreign");
        let qbittorrent = touch(&dir.join("qBittorrent").join("qbittorrent.exe"));
        let exe = dir.join("torrenta").join("torrenta.exe");
        let handler = in_latest(
            Some("qBittorrent.File.Torrent"),
            Some(format!("\"{}\" \"%1\"", qbittorrent.display())),
        );
        assert_eq!(decide(&handler, &exe), Decision::Keep);
        // Живой чужой выбор не удаляется ни при каких условиях.
        assert_eq!(broken_choice_to_delete(&handler, &exe), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_progid_in_user_choice_latest_is_claimed() {
        // Живой случай: ключ есть, `Hash` есть, `ProgId` — пустая строка,
        // поэтому Windows не открывала `.torrent` вообще.
        let exe = Path::new(r"C:\Tools\Torrenta\torrenta.exe");
        let handler = HandlerView::broken(UserChoiceKey::Latest);
        assert_eq!(
            handler.source,
            HandlerSource::EmptyChoice(UserChoiceKey::Latest)
        );
        assert_eq!(handler.source.broken_choice(), Some(UserChoiceKey::Latest));
        assert_eq!(decide(&handler, exe), Decision::Claim);
        // Такой слой лечится только удалением.
        assert_eq!(
            broken_choice_to_delete(&handler, exe),
            Some(UserChoiceKey::Latest)
        );
    }

    #[test]
    fn empty_progid_in_user_choice_is_claimed() {
        let exe = Path::new(r"C:\Tools\Torrenta\torrenta.exe");
        let handler = HandlerView::broken(UserChoiceKey::Current);
        assert_eq!(decide(&handler, exe), Decision::Claim);
        assert_eq!(
            broken_choice_to_delete(&handler, exe),
            Some(UserChoiceKey::Current)
        );
        assert_eq!(UserChoiceKey::Current.reg_path(), USER_CHOICE_KEY);
        assert_eq!(UserChoiceKey::Latest.reg_path(), USER_CHOICE_LATEST_KEY);
    }

    #[test]
    fn empty_handler_is_claimed() {
        let exe = Path::new(r"C:\Tools\Torrenta\torrenta.exe");
        // Привязки нет вовсе.
        assert_eq!(decide(&HandlerView::default(), exe), Decision::Claim);
        // Ключ есть, но пустой (так бывает после неудачной установки).
        let empty = view(Some("   "), None);
        assert_eq!(decide(&empty, exe), Decision::Claim);
        // Наш ProgId с пустой командой — это поломка, а не отсутствие привязки.
        let broken = view(Some(TORRENT_PROGID), Some("  ".to_string()));
        assert_eq!(decide(&broken, exe), Decision::RepairStalePath);
    }

    #[test]
    fn broken_foreign_handler_is_claimed() {
        let dir = unique_dir("broken");
        let gone = dir.join("removed-app").join("player.exe");
        let exe = dir.join("torrenta").join("torrenta.exe");
        let handler = view(
            Some("SomePlayer.Torrent"),
            Some(format!("\"{}\" \"%1\"", gone.display())),
        );
        assert_eq!(decide(&handler, &exe), Decision::Claim);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dead_foreign_choice_layer_is_deleted() {
        // Чужой выбор, чья программа удалена: слой глушит привязку, поэтому
        // его надо убрать, иначе регистрация Torrenta ничего не изменит.
        let dir = unique_dir("dead-choice");
        let exe = dir.join("torrenta").join("torrenta.exe");
        let handler = in_latest(
            Some("SomePlayer.Torrent"),
            Some(command_for(&dir.join("removed-app").join("player.exe"))),
        );
        assert_eq!(decide(&handler, &exe), Decision::Claim);
        assert_eq!(
            broken_choice_to_delete(&handler, &exe),
            Some(UserChoiceKey::Latest)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn drive_letter_case_and_separators_are_ignored() {
        let exe = Path::new(r"C:\Program Files\Torrenta\torrenta.exe");
        assert!(is_our_command(
            r#""c:\program files\torrenta\TORRENTA.EXE" "%1""#,
            exe
        ));
        // Прямые слэши в реестре иногда пишут установщики.
        assert!(is_our_command(
            r#""C:/Program Files/Torrenta/torrenta.exe" "%1""#,
            exe
        ));
    }

    #[test]
    fn unquoted_command_is_parsed() {
        // Путь с пробелами без кавычек: Windows всё равно его запускает,
        // значит и мы обязаны узнать свой exe.
        let exe = Path::new(r"C:\Program Files\Torrenta\torrenta.exe");
        let unquoted = r"C:\Program Files\Torrenta\torrenta.exe %1";
        assert_eq!(
            extract_exe(unquoted),
            Some(PathBuf::from(r"C:\Program Files\Torrenta\torrenta.exe"))
        );
        assert!(is_our_command(unquoted, exe));
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        // `std::env::current_exe()` отдаёт путь с префиксом `\\?\`.
        let exe = Path::new(r"\\?\C:\Tools\Torrenta\torrenta.exe");
        assert!(is_our_command(
            r#""C:\Tools\Torrenta\torrenta.exe" "%1""#,
            exe
        ));

        let unc_exe = Path::new(r"\\?\UNC\server\share\torrenta.exe");
        assert!(is_our_command(
            r#""\\server\share\torrenta.exe" "-""#,
            unc_exe
        ));
    }

    #[test]
    fn extra_arguments_are_ignored() {
        let exe = Path::new(r"C:\Tools\Torrenta\torrenta.exe");
        assert!(is_our_command(
            r#""C:\Tools\Torrenta\torrenta.exe" --open "%1""#,
            exe
        ));
        // Незакрытая кавычка — тоже встречается в криво настроенных ключах.
        assert!(is_our_command(r#""C:\Tools\Torrenta\torrenta.exe"#, exe));
    }

    #[test]
    fn auto_file_progid_needs_repair() {
        // Так было на живой машине: `.torrent` → `torrent_auto_file`, а команда
        // внутри — путь к удалённой сборке 1.3.1.
        let dir = unique_dir("autofile");
        let exe = dir.join("torrenta").join("torrenta.exe");
        let handler = view(
            Some(AUTO_FILE_PROGID),
            Some(command_for(&dir.join("old").join("torrenta.exe"))),
        );
        assert_eq!(decide(&handler, &exe), Decision::RepairStalePath);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auto_file_progid_with_live_foreign_command_is_kept() {
        // Пользователь сам выбрал живую стороннюю программу через «Открыть с помощью».
        let dir = unique_dir("autofile-foreign");
        let other = touch(&dir.join("VLC").join("vlc.exe"));
        let exe = dir.join("torrenta").join("torrenta.exe");
        let handler = view(
            Some(AUTO_FILE_PROGID),
            Some(format!("\"{}\" \"%1\"", other.display())),
        );
        assert_eq!(decide(&handler, &exe), Decision::Keep);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn foreign_handler_is_never_hijacked() {
        // Свойство, а не отдельный случай: живой чужой обработчик не
        // перехватывается ни при каком имени ProgId.
        let dir = unique_dir("property");
        let other = touch(&dir.join("foreign").join("torrent-app.exe"));
        let exe = dir.join("torrenta").join("torrenta.exe");
        let command = format!("\"{}\" \"%1\"", other.display());

        for progid in [
            "qBittorrent.File.Torrent",
            "uTorrent",
            "Transmission.Torrent",
            "SomeVendor.Product.1",
        ] {
            let handler = view(Some(progid), Some(command.clone()));
            assert_eq!(decide(&handler, &exe), Decision::Keep, "{progid}");
        }
        // И наоборот: даже наша заглушка без команды не считается рабочей.
        assert_ne!(decide(&HandlerView::default(), &exe), Decision::Keep);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn old_torrenta_under_foreign_progid_needs_repair() {
        // Чужое имя ProgId, но внутри — старая сборка Torrenta: это наша
        // запись, её надо исправить, а не заводить второй обработчик.
        let dir = unique_dir("foreign-stale");
        let exe = dir.join("torrenta").join("torrenta.exe");
        let handler = view(
            Some("Torrenta.Torrent.Old"),
            Some(command_for(&dir.join("old").join("torrenta.exe"))),
        );
        assert_eq!(decide(&handler, &exe), Decision::RepairStalePath);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn foreign_progid_pointing_at_our_deleted_exe_is_repaired() {
        // ProgId чужой, но команда ведёт на наш удалённый exe: правим путь,
        // потому что это наша же прошлая запись.
        let dir = unique_dir("foreign-deleted");
        let exe = dir.join("torrenta").join("torrenta.exe");
        let gone = dir.join("old-install").join("torrenta.exe");
        let handler = view(Some("SomeVendor.Torrent"), Some(command_for(&gone)));
        assert_eq!(decide(&handler, &exe), Decision::RepairStalePath);
        // Наш слой через чужой ProgId не удаляем — перезапись ключей достаточна.
        assert_eq!(broken_choice_to_delete(&handler, &exe), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn extract_exe_ignores_broken_commands() {
        assert_eq!(extract_exe(""), None);
        assert_eq!(extract_exe("   "), None);
        assert_eq!(extract_exe(r#""""#), None);
        assert_eq!(
            extract_exe(r#""C:\Tools\Torrenta\torrenta.exe" "%1""#),
            Some(PathBuf::from(r"C:\Tools\Torrenta\torrenta.exe"))
        );
    }

    #[test]
    fn looks_like_torrenta_detects_stale_builds() {
        assert!(looks_like_torrenta(Path::new(
            r"C:\Old\Torrenta\torrenta.exe"
        )));
        assert!(looks_like_torrenta(Path::new(r"C:\Old\Torrenta-1.3.1.exe")));
        assert!(!looks_like_torrenta(Path::new(
            r"C:\Program Files\qBittorrent\qbittorrent.exe"
        )));
        assert!(!looks_like_torrenta(Path::new(r"C:\Tools\player.exe")));
    }

    #[test]
    fn our_progids_are_recognized_with_any_case() {
        assert!(is_our_progid(APPLICATIONS_PROGID));
        assert!(is_our_progid("applications\\TORRENTA.EXE"));
        assert!(is_our_progid(TORRENT_PROGID));
        assert!(is_our_progid("torrenta.TORRENT"));
        assert!(is_our_progid(MAGNET_PROGID));
        assert!(is_our_progid(AUTO_FILE_PROGID));
        assert!(!is_our_progid("qBittorrent.File.Torrent"));
        assert!(!is_our_progid(""));
    }

    #[test]
    fn command_for_quotes_exe_and_argument() {
        assert_eq!(
            command_for(Path::new(r"C:\Program Files\Torrenta\torrenta.exe")),
            r#""C:\Program Files\Torrenta\torrenta.exe" "%1""#
        );
        assert_eq!(
            icon_for(Path::new(r"C:\Tools\torrenta.exe")),
            r#""C:\Tools\torrenta.exe",0"#
        );
    }

    #[test]
    fn handler_is_ours_compares_command_not_progid() {
        let exe = Path::new(r"C:\Tools\Torrenta\torrenta.exe");
        // Наш ProgId, но команда на старый путь: владельцем не считаем.
        let stale = view(
            Some(TORRENT_PROGID),
            Some(r#""C:\Old\torrenta.exe" "%1""#.to_string()),
        );
        assert!(!handler_is_ours(&stale, exe));
        assert!(handler_is_ours(&own_handler(exe), exe));
        assert!(!handler_is_ours(&HandlerView::default(), exe));
    }

    #[test]
    fn hint_explains_foreign_and_broken_state() {
        let dir = unique_dir("hint");
        let exe = dir.join("torrenta").join("torrenta.exe");
        let qbittorrent = touch(&dir.join("qBittorrent").join("qbittorrent.exe"));

        // Свой обработчик — пояснений не нужно.
        assert_eq!(hint_for(&own_handler(&exe), &exe, true), None);

        // Живой чужой: подсказываем, где переключить программу.
        let foreign = view(
            Some("qBittorrent.File.Torrent"),
            Some(command_for(&qbittorrent)),
        );
        let hint = hint_for(&foreign, &exe, false).expect("подсказка есть");
        assert!(hint.contains("qbittorrent"), "{hint}");
        assert!(hint.contains("Приложения по умолчанию"), "{hint}");

        // Сломанный слой `UserChoice*`: сообщаем про пустой ProgId.
        let broken = HandlerView::broken(UserChoiceKey::Latest);
        let hint = hint_for(&broken, &exe, false).expect("подсказка есть");
        assert!(hint.contains("пустой ProgId"), "{hint}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unavailable_state_carries_reason() {
        let state = AssociationState::unavailable("реестр недоступен".to_string());
        assert!(!state.is_owner);
        assert!(!state.repaired);
        assert_eq!(state.message.as_deref(), Some("реестр недоступен"));
        assert_eq!(state.handler, None);
    }
}
