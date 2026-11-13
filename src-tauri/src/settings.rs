//! Настройки приложения Torrenta.
//!
//! Модуль отвечает за три вещи:
//! 1. DTO настроек, который ходит между фронтендом, диском и движком
//!    (`SettingsDto` ⇄ `EngineConfig`).
//! 2. Нормализацию и валидацию пользовательского ввода (никаких паник,
//!    только понятные русские ошибки).
//! 3. Хранение в `settings.json` рядом с каталогом конфигурации приложения
//!    с атомарной записью (временный файл + `rename`), чтобы при падении
//!    приложения настройки не превратились в обрезанный JSON.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use torrent_core::EngineConfig;

/// Имя файла настроек в каталоге конфигурации приложения.
pub const SETTINGS_FILE_NAME: &str = "settings.json";

/// Число пиров на торрент по умолчанию (совпадает с `EngineConfig::default()`).
///
/// Раньше здесь было 50, и это упирало скорость в потолок: при одном куске на
/// пира десяток медленных соединений не разгонял торрент, даже когда трекер
/// отдавал сотни адресов. 200 — как у libtorrent по умолчанию на торрент.
pub const DEFAULT_MAX_PEERS: u32 = 200;
/// Прежнее значение по умолчанию — для миграции уже установленных клиентов.
///
/// До 1.8.0 умолчанием было 50, и ровно это число попадало в `settings.json`
/// при первом сохранении. Если оставить его как есть, обновление ничего не
/// изменит: файл настроек перекрывает новое умолчание, и пользователь так и
/// сидит на 50 соединениях. Поэтому в [`migrate_legacy_defaults`] ровно 50
/// (и только оно) считается «старым умолчанием».
pub const LEGACY_DEFAULT_MAX_PEERS: u32 = 50;
/// Верхняя граница числа пиров на торрент.
pub const MAX_PEERS_LIMIT: u32 = 500;
/// Верхняя граница одновременно активных загрузок и раздач.
///
/// `0` означает «без ограничения» и тоже допустим — зажимается только верх.
pub const MAX_ACTIVE_LIMIT: u32 = 100;
/// Верхняя граница лимита скорости — 1 ГиБ/с (в КиБ/с).
pub const MAX_RATE_LIMIT_KIB: u32 = 1_048_576;
/// Верхняя граница listen-порта: `0` означает «выбрать свободный».
pub const MAX_LISTEN_PORT: u16 = u16::MAX;
/// Схемы прокси, которые принимает приложение (в нижнем регистре).
///
/// Список повторяет разбор движка ([`torrent_core::proxy::ProxyConfig::parse`]):
/// приложение не придумывает собственных правил, поэтому всё, что понимает
/// движок, сохраняется без изменений, и наоборот.
pub const PROXY_SCHEMES: [&str; 4] = ["socks5://", "socks5h://", "http://", "https://"];
/// Допустимые темы оформления.
pub const THEMES: [&str; 2] = ["dark", "light"];
/// Тема оформления по умолчанию.
pub const DEFAULT_THEME: &str = "dark";
/// Минут в сутках: границы окна расписания задаются временем внутри суток.
pub const MINUTES_PER_DAY: u32 = 1440;
/// Имя файла-маркера портативного режима рядом с `torrenta.exe`.
pub const PORTABLE_MARKER: &str = "torrenta-portable.txt";
/// Второе допустимое имя маркера (без расширения) — принимаются оба варианта.
pub const PORTABLE_MARKER_ALT: &str = "portable";
/// Каталог данных в портативном режиме (рядом с `torrenta.exe`).
pub const PORTABLE_DATA_DIR_NAME: &str = "TorrentaData";

/// Настройки в том виде, в котором их видит фронтенд.
///
/// Все поля в `snake_case` — это контракт с фронтендом, договорённый в
/// `docs/PRODUCT.md` / задаче; переименовывать их без согласования нельзя.
///
/// `Eq` не выводится сознательно: среди полей есть `f64` (`seed_ratio_limit`),
/// а сравнение чисел с плавающей точкой на равенство некорректно (`NaN`).
/// Сравнение настроек идёт через `PartialEq`, а `Hash` для отпечатков
/// состояния реализован вручную (см. ниже).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SettingsDto {
    /// Каталог, куда сохраняются данные торрентов. Абсолютный путь.
    pub download_dir: String,
    /// Максимальное число пиров на торрент (1..=500).
    pub max_peers: u32,
    /// Сколько торрентов одновременно могут скачиваться (0..=100, `0` — без
    /// ограничения). Остальные ждут в очереди с состоянием «В очереди».
    pub max_active_downloads: u32,
    /// Сколько торрентов одновременно могут раздаваться (0..=100, `0` — без
    /// ограничения). Завершённые сверх лимита тоже ждут в очереди.
    pub max_active_seeds: u32,
    /// Ограничение скорости загрузки в КиБ/с, `0` — без лимита.
    pub download_limit_kib: u32,
    /// Ограничение скорости отдачи в КиБ/с, `0` — без лимита.
    pub upload_limit_kib: u32,
    /// Порт для входящих подключений, `0` — свободный порт.
    pub listen_port: u16,
    /// Включить DHT (BEP 5): поиск пиров без трекера.
    pub dht: bool,
    /// UDP-порт DHT, `0` — любой свободный.
    ///
    /// Порт DHT не связан с [`SettingsDto::listen_port`] из TCP: движок слушает
    /// его отдельным UDP-сокетом, и нулевое значение выбирает свободный порт.
    pub dht_port: u16,
    /// Включить PEX (BEP 11): обмен пирами через подключённых соседей.
    pub pex: bool,
    /// Прокси для исходящих TCP-соединений; пустая строка — прокси выключен.
    ///
    /// Поддерживаются схемы `socks5://`, `http://` и `https://`, дальше обязателен
    /// `хост:порт`; логин и пароль необязательны и пишутся перед хостом через `@`
    /// (`socks5://user:pass@127.0.0.1:1080`). Строка неверного формата не ломает
    /// настройки: нормализация сбрасывает её в пустую и пишет причину в лог.
    pub proxy: String,
    /// Пробрасывать порт на роутере через UPnP-IGD или NAT-PMP.
    pub port_mapping: bool,
    /// Использовать web seeds из `url-list` торрента (BEP 19).
    pub web_seeds: bool,
    /// Раздавать после завершения загрузки.
    pub seed_after_complete: bool,
    /// Останавливать раздачу при достижении ratio `N` (`0` — без ограничения).
    ///
    /// Раздача прекращается, когда отдано не меньше `N` объёма загрузки; значение
    /// меньше нуля и `NaN`/бесконечность считаются выключенным ограничением.
    pub seed_ratio_limit: f64,
    /// Останавливать раздачу через N минут после завершения (`0` — без ограничения).
    pub seed_time_limit_min: u32,
    /// Скачивать новые торренты последовательно (куски по порядку).
    ///
    /// Это значение по умолчанию для добавляемых торрентов: у отдельного торрента
    /// порядок меняет команда `set_torrent_sequential`, и её выбор живёт в сессии,
    /// а не здесь.
    pub sequential: bool,
    /// Включать расписание скорости.
    ///
    /// Когда флаг включён, внутри окна [`SettingsDto::schedule_from_min`] —
    /// [`SettingsDto::schedule_to_min`] действуют лимиты `schedule_*_limit_kib`,
    /// а вне окна — обычные [`SettingsDto::download_limit_kib`] и
    /// [`SettingsDto::upload_limit_kib`]. Окно с равными границами считается
    /// выключенным расписанием: нормализация снимает флаг.
    pub schedule_enabled: bool,
    /// Начало окна расписания: минуты от полуночи (`0..=1439`).
    pub schedule_from_min: u32,
    /// Конец окна расписания: минуты от полуночи (`0..=1439`).
    ///
    /// Если конец меньше начала, окно переходит через полночь (`22:00`–`06:00`).
    pub schedule_to_min: u32,
    /// Лимит загрузки внутри окна расписания, КиБ/с (`0` — без лимита).
    pub schedule_download_limit_kib: u32,
    /// Лимит отдачи внутри окна расписания, КиБ/с (`0` — без лимита).
    pub schedule_upload_limit_kib: u32,
    /// Выключать компьютер, когда все загрузки завершены.
    ///
    /// Само выключение выполняет наблюдатель движка (см. `auto_shutdown`),
    /// настройка лишь хранит выбор пользователя между запусками.
    pub auto_shutdown: bool,
    /// Запускать Torrenta вместе с Windows.
    ///
    /// Значение хранится в `settings.json`, а сама запись в реестр
    /// (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`) выполняется
    /// модулем [`crate::autostart`] при старте и при сохранении настроек.
    pub autostart: bool,
    /// Регистрировать Torrenta обработчиком `.torrent`-файлов.
    ///
    /// Когда флаг включён, при старте приложение проверяет привязку расширения в
    /// реестре и исправляет её, если она ведёт на старый (уже удалённый) exe или
    /// сломана пустым `ProgId` (см. [`crate::association`]). Живой чужой
    /// обработчик — например, qBittorrent — при этом не перехватывается.
    pub associate_files: bool,
    /// Запускать приложение сразу в трее, не показывая окно.
    ///
    /// Настройка действует только на старте: если окно не показано, его можно
    /// открыть из меню иконки в трее.
    pub start_minimized: bool,
    /// Закрывать окно в трей вместо выхода из приложения.
    ///
    /// Когда флаг включён, крестик окна прячет окно, а приложение продолжает
    /// работать в трее; выход выполняет пункт «Выход» меню трея.
    pub close_to_tray: bool,
    /// Показывать уведомление Windows, когда загрузка завершилась.
    ///
    /// Уведомление появляется, только если главное окно не на переднем плане;
    /// при выключенном флаге никаких уведомлений не показывается.
    pub notify_on_complete: bool,
    /// Папка наблюдения за `.torrent`-файлами; пустая строка — наблюдение выключено.
    ///
    /// Если путь задан, приложение раз в несколько секунд просматривает папку и
    /// автоматически добавляет новые `.torrent`, перенося их в подкаталог `done`
    /// (см. [`crate::watch`]). Путь должен быть абсолютным.
    pub watch_dir: String,
    /// Восстанавливать список торрентов при следующем запуске.
    ///
    /// Когда флаг выключен, сессия не сохраняется и не читается: после
    /// перезапуска список торрентов будет пустым.
    pub restore_session: bool,
    /// Тема оформления: `"dark"` или `"light"`.
    pub theme: String,
}

/// Хеш настроек для отпечатков состояния (см. `AppStateDto::fingerprints`).
///
/// Реализован вручную, потому что `Hash` нельзя вывести вместе с `f64`-полем:
/// для `seed_ratio_limit` хешируются биты. `Eq` при этом не реализуется —
/// для чисел с плавающей точкой сравнение на равенство даёт `false` для `NaN`,
/// а хеш нужен только как «отпечаток изменившихся настроек».
impl std::hash::Hash for SettingsDto {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.download_dir.hash(state);
        self.max_peers.hash(state);
        self.max_active_downloads.hash(state);
        self.max_active_seeds.hash(state);
        self.download_limit_kib.hash(state);
        self.upload_limit_kib.hash(state);
        self.listen_port.hash(state);
        self.dht.hash(state);
        self.dht_port.hash(state);
        self.pex.hash(state);
        self.proxy.hash(state);
        self.port_mapping.hash(state);
        self.web_seeds.hash(state);
        self.seed_after_complete.hash(state);
        self.seed_ratio_limit.to_bits().hash(state);
        self.seed_time_limit_min.hash(state);
        self.sequential.hash(state);
        self.schedule_enabled.hash(state);
        self.schedule_from_min.hash(state);
        self.schedule_to_min.hash(state);
        self.schedule_download_limit_kib.hash(state);
        self.schedule_upload_limit_kib.hash(state);
        self.auto_shutdown.hash(state);
        self.autostart.hash(state);
        self.associate_files.hash(state);
        self.start_minimized.hash(state);
        self.close_to_tray.hash(state);
        self.notify_on_complete.hash(state);
        self.watch_dir.hash(state);
        self.restore_session.hash(state);
        self.theme.hash(state);
    }
}

impl Default for SettingsDto {
    fn default() -> Self {
        Self {
            download_dir: String::new(),
            max_peers: DEFAULT_MAX_PEERS,
            max_active_downloads: 0,
            max_active_seeds: 0,
            download_limit_kib: 0,
            upload_limit_kib: 0,
            listen_port: 0,
            dht: true,
            dht_port: 0,
            pex: true,
            proxy: String::new(),
            port_mapping: true,
            web_seeds: true,
            seed_after_complete: true,
            seed_ratio_limit: 0.0,
            seed_time_limit_min: 0,
            // Новые торренты качаются в обычном (не последовательном) порядке,
            // расписание скорости выключено: это поведение 1.6.
            sequential: false,
            schedule_enabled: false,
            schedule_from_min: 0,
            schedule_to_min: 0,
            schedule_download_limit_kib: 0,
            schedule_upload_limit_kib: 0,
            auto_shutdown: false,
            autostart: false,
            // Привязка `.torrent` включается по умолчанию: иначе после обновления
            // файлы открывались бы старым (несуществующим) exe.
            associate_files: true,
            start_minimized: false,
            close_to_tray: false,
            notify_on_complete: true,
            watch_dir: String::new(),
            restore_session: true,
            theme: DEFAULT_THEME.to_string(),
        }
    }
}

impl SettingsDto {
    /// Настройки с указанным каталогом загрузки и остальными значениями
    /// по умолчанию.
    pub fn with_download_dir(dir: impl Into<String>) -> Self {
        Self {
            download_dir: dir.into(),
            ..Self::default()
        }
    }

    /// Приводит настройки в допустимый диапазон «на месте».
    ///
    /// Функция идемпотентна: `normalize(normalize(x)) == normalize(x)`.
    pub fn normalize(&mut self) {
        self.download_dir = self.download_dir.trim().to_string();
        self.watch_dir = self.watch_dir.trim().to_string();
        self.max_peers = clamp_peers(self.max_peers);
        self.max_active_downloads = clamp_active(self.max_active_downloads);
        self.max_active_seeds = clamp_active(self.max_active_seeds);
        self.download_limit_kib = clamp_rate(self.download_limit_kib);
        self.upload_limit_kib = clamp_rate(self.upload_limit_kib);
        self.proxy = normalize_proxy(&self.proxy);
        self.seed_ratio_limit = normalize_seed_ratio(self.seed_ratio_limit);
        // `dht_port` и `seed_time_limit_min` зажимать не нужно: их типы (`u16`
        // и `u32`) и есть допустимый диапазон, а `0` — законное «без значения».
        self.theme = normalize_theme(&self.theme);
        // Расписание: границы окна — минуты внутри суток, лимиты — те же, что у
        // обычных (1 ГиБ/с), а окно нулевой длины расписанием не считается.
        self.schedule_from_min %= MINUTES_PER_DAY;
        self.schedule_to_min %= MINUTES_PER_DAY;
        self.schedule_download_limit_kib = clamp_rate(self.schedule_download_limit_kib);
        self.schedule_upload_limit_kib = clamp_rate(self.schedule_upload_limit_kib);
        if self.schedule_from_min == self.schedule_to_min {
            self.schedule_enabled = false;
        }
    }

    /// То же, что [`SettingsDto::normalize`], но по цепочке вызовов.
    pub fn normalized(mut self) -> Self {
        self.normalize();
        self
    }

    /// Абсолютный путь каталога загрузки (для работы с файловой системой).
    pub fn download_path(&self) -> PathBuf {
        PathBuf::from(&self.download_dir)
    }
}

/// Зажимает число пиров в допустимый диапазон.
pub fn clamp_peers(value: u32) -> u32 {
    value.clamp(1, MAX_PEERS_LIMIT)
}

/// Зажимает лимит скорости в допустимый диапазон (`0` = без лимита).
pub fn clamp_rate(value: u32) -> u32 {
    value.min(MAX_RATE_LIMIT_KIB)
}

/// Зажимает лимит одновременно активных торрентов (`0` = без ограничения).
pub fn clamp_active(value: u32) -> u32 {
    value.min(MAX_ACTIVE_LIMIT)
}

/// Приводит название темы к одному из [`THEMES`]; всё неизвестное — `dark`.
pub fn normalize_theme(raw: &str) -> String {
    let lowered = raw.trim().to_ascii_lowercase();
    if THEMES.contains(&lowered.as_str()) {
        lowered
    } else {
        DEFAULT_THEME.to_string()
    }
}

/// Приводит лимит ratio к неотрицательному конечному числу.
///
/// `0.0` означает «без ограничения». Отрицательное число, `NaN` и бесконечность
/// не имеют смысла и могли попасть в `settings.json` только правкой вручную,
/// поэтому молча становятся `0.0`: из-за опечатки в одном числе приложение не
/// должно терять остальные настройки.
pub fn normalize_seed_ratio(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

/// Приводит строку прокси к пригодному виду.
///
/// Пустая строка — прокси выключен. Строка обрезается по краям и проверяется
/// разбором движка ([`torrent_core::proxy::ProxyConfig::parse`]), поэтому
/// правила приложения и движка не расходятся: допустимы схемы из
/// [`PROXY_SCHEMES`], логин и пароль пишутся перед хостом через `@`
/// (`socks5://логин:пароль@хост:порт`), порт можно опустить — движок подставит
/// 1080 для SOCKS5 и 8080 для HTTP. Строка неверного формата сбрасывается в
/// пустую, чтобы опечатка не оставила пользователя без сети, а причина уходит
/// в лог.
pub fn normalize_proxy(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // Разбор общий с движком: `torrent_core::proxy::ProxyConfig::parse` уже
    // проверяет схему, хост и порт и возвращает причину на русском.
    match torrent_core::proxy::ProxyConfig::parse(trimmed) {
        Ok(_) => trimmed.to_string(),
        Err(err) => {
            eprintln!(
                "[torrenta] прокси «{trimmed}» отключён: {err}; поддерживаются схемы {} \
                 (логин:пароль@ необязателен, порт по умолчанию — 1080 у SOCKS5 и 8080 у HTTP)",
                PROXY_SCHEMES.join(", ")
            );
            String::new()
        }
    }
}

/// Полная нормализация «сырых» настроек: значения в диапазонах, каталог
/// загрузки не пустой (подставляется `fallback_dir`).
pub fn normalize_with_fallback(mut dto: SettingsDto, fallback_dir: &Path) -> SettingsDto {
    dto.normalize();
    if dto.download_dir.is_empty() {
        dto.download_dir = fallback_dir.to_string_lossy().into_owned();
    }
    dto
}

/// Готовит настройки к применению: нормализует, проверяет каталог загрузки и
/// создаёт его при необходимости.
///
/// Возвращает нормализованный DTO либо человеческую ошибку на русском.
pub fn prepare(dto: SettingsDto, fallback_dir: &Path) -> Result<SettingsDto, String> {
    let dto = normalize_with_fallback(dto, fallback_dir);
    if dto.download_dir.is_empty() {
        return Err("не удалось определить каталог загрузки: укажите путь вручную".to_string());
    }

    let dir = dto.download_path();
    if !dir.is_absolute() {
        return Err(format!(
            "каталог загрузки «{}» должен быть абсолютным путём (например, C:\\Загрузки)",
            dto.download_dir
        ));
    }

    if dir.exists() && !dir.is_dir() {
        return Err(format!("«{}» существует, но это не каталог", dir.display()));
    }

    if !dir.is_dir() {
        fs::create_dir_all(&dir).map_err(|err| {
            format!(
                "не удалось создать каталог загрузки «{}»: {err}",
                dir.display()
            )
        })?;
    }

    if !dir.is_dir() {
        return Err(format!("«{}» существует, но это не каталог", dir.display()));
    }

    prepare_watch_dir(&dto.watch_dir)?;

    Ok(dto)
}

/// Проверяет папку наблюдения и создаёт её при необходимости.
///
/// Пустой путь — наблюдение выключено, проверять нечего. Ошибки на русском,
/// чтобы их можно было показать пользователю без перевода.
pub fn prepare_watch_dir(raw: &str) -> Result<(), String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(());
    }

    let dir = Path::new(raw);
    if !dir.is_absolute() {
        return Err(format!(
            "папка наблюдения «{raw}» должна быть абсолютным путём (например, C:\\Загрузки\\torrents)"
        ));
    }

    if dir.exists() && !dir.is_dir() {
        return Err(format!(
            "«{}» существует, но это не каталог: выберите другую папку наблюдения",
            dir.display()
        ));
    }

    if !dir.is_dir() {
        fs::create_dir_all(dir).map_err(|err| {
            format!(
                "не удалось создать папку наблюдения «{}»: {err}",
                dir.display()
            )
        })?;
    }

    if !dir.is_dir() {
        return Err(format!(
            "«{}» существует, но это не каталог: выберите другую папку наблюдения",
            dir.display()
        ));
    }

    Ok(())
}

/// Преобразует настройки в конфигурацию движка.
///
/// Значения должны быть уже нормализованы ([`prepare`] / [`SettingsDto::normalize`]),
/// функция лишь переносит поля и защищает от переполнения `usize`.
pub fn to_engine_config(dto: &SettingsDto) -> EngineConfig {
    EngineConfig {
        download_dir: dto.download_path(),
        max_peers_per_torrent: dto.max_peers as usize,
        download_limit_kib: dto.download_limit_kib,
        upload_limit_kib: dto.upload_limit_kib,
        listen_port: dto.listen_port,
        seed_after_complete: dto.seed_after_complete,
        max_active_downloads: dto.max_active_downloads as usize,
        max_active_seeds: dto.max_active_seeds as usize,
        dht: dto.dht,
        dht_port: dto.dht_port,
        pex: dto.pex,
        web_seeds: dto.web_seeds,
        // Пустая строка в настройках — «прокси выключен», движок ждёт `None`.
        proxy: if dto.proxy.is_empty() {
            None
        } else {
            Some(dto.proxy.clone())
        },
        port_mapping: dto.port_mapping,
        seed_ratio_limit: dto.seed_ratio_limit,
        seed_time_limit_min: dto.seed_time_limit_min,
        sequential: dto.sequential,
        schedule_enabled: dto.schedule_enabled,
        schedule_from_min: dto.schedule_from_min,
        schedule_to_min: dto.schedule_to_min,
        schedule_download_limit_kib: dto.schedule_download_limit_kib,
        schedule_upload_limit_kib: dto.schedule_upload_limit_kib,
    }
}

/// Обратное преобразование: конфигурация движка → DTO настроек.
///
/// Тема, автовыключение и «оконные» флаги (`autostart`, `start_minimized`,
/// `close_to_tray`) не входят в `EngineConfig`, поэтому передаются отдельно:
/// первые два — аргументами, остальные всегда выключены — их хранит только
/// `settings.json`, поэтому при обратном чтении их нужно брать с диска.
pub fn from_engine_config(config: &EngineConfig, theme: &str, auto_shutdown: bool) -> SettingsDto {
    SettingsDto {
        download_dir: config.download_dir.to_string_lossy().into_owned(),
        max_peers: u32::try_from(config.max_peers_per_torrent)
            .unwrap_or(MAX_PEERS_LIMIT)
            .clamp(1, MAX_PEERS_LIMIT),
        download_limit_kib: clamp_rate(config.download_limit_kib),
        upload_limit_kib: clamp_rate(config.upload_limit_kib),
        listen_port: config.listen_port,
        dht: config.dht,
        dht_port: config.dht_port,
        pex: config.pex,
        // Движок хранит прокси как `Option<String>`: `None` — выключено, а в
        // настройках это пустая строка. Значение движка уже проверено в
        // `EngineConfig::sanitized`, но нормализация здесь бесплатна и держит
        // инвариант DTO.
        proxy: normalize_proxy(config.proxy.as_deref().unwrap_or("")),
        port_mapping: config.port_mapping,
        web_seeds: config.web_seeds,
        seed_after_complete: config.seed_after_complete,
        seed_ratio_limit: normalize_seed_ratio(config.seed_ratio_limit),
        seed_time_limit_min: config.seed_time_limit_min,
        sequential: config.sequential,
        schedule_enabled: config.schedule_enabled,
        // Границы окна движок уже держит внутри суток (`sanitized`), лимиты —
        // в допустимом диапазоне; нормализация повторяет инвариант DTO.
        schedule_from_min: config.schedule_from_min % MINUTES_PER_DAY,
        schedule_to_min: config.schedule_to_min % MINUTES_PER_DAY,
        schedule_download_limit_kib: clamp_rate(config.schedule_download_limit_kib),
        schedule_upload_limit_kib: clamp_rate(config.schedule_upload_limit_kib),
        max_active_downloads: clamp_active(
            u32::try_from(config.max_active_downloads).unwrap_or(MAX_ACTIVE_LIMIT),
        ),
        max_active_seeds: clamp_active(
            u32::try_from(config.max_active_seeds).unwrap_or(MAX_ACTIVE_LIMIT),
        ),
        auto_shutdown,
        autostart: false,
        // Привязка `.torrent` не входит в `EngineConfig`: управляет ею реестр,
        // поэтому обратное преобразование отдаёт значение по умолчанию.
        associate_files: true,
        start_minimized: false,
        close_to_tray: false,
        // Настройки уведомлений, наблюдения за папкой и восстановления сессии
        // не входят в `EngineConfig`: их хранит только `settings.json`.
        notify_on_complete: true,
        watch_dir: String::new(),
        restore_session: true,
        theme: normalize_theme(theme),
    }
}

/// Путь к файлу настроек внутри каталога конфигурации приложения.
pub fn settings_path(app_dir: &Path) -> PathBuf {
    app_dir.join(SETTINGS_FILE_NAME)
}

/// Итог разрешения каталога данных: сам путь и признак портативного режима.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir {
    /// Каталог, в котором живут `settings.json` и подкаталог `session/`.
    pub dir: PathBuf,
    /// `true`, если данные лежат рядом с исполняемым файлом (портативный режим).
    pub portable: bool,
}

/// Определяет каталог данных приложения — единая точка выбора для всех модулей.
///
/// Результат получают настройки ([`settings_path`]), сессия
/// ([`crate::session::SessionRegistry`]) и состояние ([`crate::state::AppEngine`]),
/// поэтому все файлы приложения лежат в одном месте: либо в каталоге конфигурации
/// Tauri (`app_dir`, обычно `%APPDATA%\Torrenta`), либо рядом с `torrenta.exe`.
///
/// Портативный режим включается файлом-маркером [`PORTABLE_MARKER`] рядом с
/// исполняемым файлом (принимается и [`PORTABLE_MARKER_ALT`]). Данные при этом
/// складываются в подкаталог [`PORTABLE_DATA_DIR_NAME`]: отдельный каталог можно
/// целиком скопировать на флешку, и он не конфликтует с файлами, которые лежат
/// рядом с exe.
///
/// Каталог создаётся при первом обращении. Если рядом с exe нет прав на запись,
/// паники не будет: причина уходит в stderr, приложение остаётся на `app_dir`
/// с `portable: false` — запуск важнее портативности.
pub fn resolve_data_dir(exe_dir: Option<&Path>, app_dir: &Path) -> DataDir {
    let fallback = DataDir {
        dir: app_dir.to_path_buf(),
        portable: false,
    };

    let Some(exe_dir) = exe_dir else {
        return fallback;
    };
    if !has_portable_marker(exe_dir) {
        return fallback;
    }

    let dir = exe_dir.join(PORTABLE_DATA_DIR_NAME);
    match ensure_writable_dir(&dir) {
        Ok(()) => DataDir {
            dir,
            portable: true,
        },
        Err(err) => {
            eprintln!(
                "[torrenta] портативный режим недоступен: каталог «{}» использовать нельзя ({err}); данные останутся в «{}»",
                dir.display(),
                app_dir.display()
            );
            fallback
        }
    }
}

/// Проверяет, что рядом с исполняемым файлом лежит маркер портативного режима.
pub fn has_portable_marker(exe_dir: &Path) -> bool {
    [PORTABLE_MARKER, PORTABLE_MARKER_ALT]
        .iter()
        .any(|name| exe_dir.join(name).is_file())
}

/// Создаёт каталог данных и убеждается, что в него действительно можно писать.
///
/// Проверка записью нужна потому, что `create_dir_all` умеет «успешно» вернуть
/// `Ok`, даже когда каталог уже есть, а прав на запись в него нет: без пробного
/// файла первый же `save` упал бы уже во время работы приложения.
fn ensure_writable_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|err| format!("не удалось создать каталог: {err}"))?;
    if !dir.is_dir() {
        return Err("путь занят файлом".to_string());
    }

    let probe = dir.join(".torrenta-write-test");
    fs::write(&probe, b"ok").map_err(|err| format!("нет прав на запись: {err}"))?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

/// Читает настройки с диска, никогда не падая.
///
/// Любая проблема (нет файла, битый JSON, нет прав) заканчивается настройками
/// по умолчанию и сообщением в stderr: приложение должно запускаться всегда.
pub fn load(path: &Path, fallback_dir: &Path) -> SettingsDto {
    let stored = match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<SettingsDto>(&text) {
            Ok(dto) => Some(dto),
            Err(err) => {
                eprintln!(
                    "[torrenta] файл настроек «{}» повреждён ({err}), использую значения по умолчанию",
                    path.display()
                );
                None
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            eprintln!(
                "[torrenta] не удалось прочитать настройки «{}»: {err}",
                path.display()
            );
            None
        }
    };

    let from_disk = stored.is_some();
    let mut dto = stored.unwrap_or_default();
    if from_disk {
        // Миграция нужна только для файла, который уже лежит на диске: свежие
        // настройки и так получают актуальные значения по умолчанию.
        migrate_legacy_defaults(&mut dto);
    }
    normalize_with_fallback(dto, fallback_dir)
}

/// Поднимает устаревшие значения по умолчанию до актуальных.
///
/// Пока трогаем одно поле: число пиров на торрент. В 1.7.0 и раньше умолчание
/// было 50, и это же число сохранялось в `settings.json` при любом изменении
/// других настроек. После 1.8.0 умолчание — 200; без миграции обновлённый
/// клиент продолжал бы держать 50 соединений и не давал обещанного прироста.
///
/// Мигрируем только точное совпадение со старым умолчанием: если пользователь
/// выбрал 30 или 100, это его осознанный выбор и мы его не переписываем.
pub fn migrate_legacy_defaults(dto: &mut SettingsDto) {
    if dto.max_peers == LEGACY_DEFAULT_MAX_PEERS {
        dto.max_peers = DEFAULT_MAX_PEERS;
    }
}

/// Атомарно сохраняет настройки в JSON.
///
/// Данные пишутся во временный файл рядом с целевым и затем переименовываются:
/// на диске всегда либо старый, либо новый полный JSON.
pub fn save(path: &Path, dto: &SettingsDto) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|err| {
            format!(
                "не удалось создать каталог настроек «{}»: {err}",
                dir.display()
            )
        })?;
    }

    let json = serde_json::to_string_pretty(dto)
        .map_err(|err| format!("не удалось сериализовать настройки: {err}"))?;

    let tmp_path = temporary_path(path);
    {
        let mut file = fs::File::create(&tmp_path).map_err(|err| {
            format!(
                "не удалось создать временный файл настроек «{}»: {err}",
                tmp_path.display()
            )
        })?;
        file.write_all(json.as_bytes())
            .map_err(|err| format!("не удалось записать настройки: {err}"))?;
        file.write_all(b"\n")
            .map_err(|err| format!("не удалось записать настройки: {err}"))?;
        file.sync_all()
            .map_err(|err| format!("не удалось сохранить настройки на диск: {err}"))?;
    }

    fs::rename(&tmp_path, path).map_err(|err| {
        let _ = fs::remove_file(&tmp_path);
        format!(
            "не удалось сохранить настройки в «{}»: {err}",
            path.display()
        )
    })
}

/// Путь временного файла для атомарной записи (в том же каталоге).
fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| SETTINGS_FILE_NAME.to_string());
    path.with_file_name(format!(".{name}.tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use torrent_core::EngineConfig;

    /// Уникальный временный каталог для одного теста.
    fn test_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "torrenta-settings-{tag}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    fn sample() -> SettingsDto {
        SettingsDto {
            download_dir: "C:\\Torrents".to_string(),
            max_peers: 80,
            max_active_downloads: 4,
            max_active_seeds: 2,
            download_limit_kib: 4096,
            upload_limit_kib: 512,
            listen_port: 51413,
            // P2-поля пока не входят в `EngineConfig`, поэтому в образце они
            // остаются по умолчанию; их поведение проверяют отдельные тесты
            // (`p2_*`), где значения заведомо отличаются от дефолтных.
            dht: true,
            dht_port: 0,
            pex: true,
            proxy: String::new(),
            port_mapping: true,
            web_seeds: true,
            seed_ratio_limit: 0.0,
            seed_time_limit_min: 0,
            sequential: true,
            schedule_enabled: true,
            schedule_from_min: 60,
            schedule_to_min: 90,
            schedule_download_limit_kib: 2048,
            schedule_upload_limit_kib: 128,
            seed_after_complete: false,
            auto_shutdown: true,
            autostart: true,
            associate_files: true,
            start_minimized: true,
            close_to_tray: true,
            notify_on_complete: false,
            watch_dir: "C:\\Torrents\\watch".to_string(),
            restore_session: false,
            theme: "light".to_string(),
        }
    }

    #[test]
    fn defaults_are_sane() {
        let dto = SettingsDto::default();
        assert_eq!(dto.max_peers, DEFAULT_MAX_PEERS);
        assert_eq!(dto.max_active_downloads, 0);
        assert_eq!(dto.max_active_seeds, 0);
        assert_eq!(dto.download_limit_kib, 0);
        assert_eq!(dto.upload_limit_kib, 0);
        assert_eq!(dto.listen_port, 0);
        assert!(dto.seed_after_complete);
        assert!(!dto.auto_shutdown);
        assert!(!dto.autostart);
        assert!(!dto.start_minimized);
        assert!(!dto.close_to_tray);
        assert!(dto.notify_on_complete);
        assert!(dto.restore_session);
        assert!(dto.watch_dir.is_empty());
        assert_eq!(dto.theme, DEFAULT_THEME);
        assert!(dto.download_dir.is_empty());
        assert!(
            !dto.sequential,
            "обычный порядок загрузки — поведение по умолчанию"
        );
        assert!(!dto.schedule_enabled, "расписание скорости выключено");
        assert_eq!(dto.schedule_from_min, 0);
        assert_eq!(dto.schedule_to_min, 0);
        assert_eq!(dto.schedule_download_limit_kib, 0);
        assert_eq!(dto.schedule_upload_limit_kib, 0);
        assert_eq!(dto, dto.clone().normalized());
    }

    #[test]
    fn normalize_clamps_out_of_range_values() {
        let mut dto = SettingsDto {
            download_dir: "   C:\\Torrents   ".to_string(),
            max_peers: 0,
            max_active_downloads: u32::MAX,
            max_active_seeds: MAX_ACTIVE_LIMIT + 1,
            download_limit_kib: u32::MAX,
            upload_limit_kib: MAX_RATE_LIMIT_KIB + 1,
            listen_port: 65_535,
            dht: false,
            dht_port: 54_321,
            pex: false,
            proxy: "  socks5://127.0.0.1:1080  ".to_string(),
            port_mapping: false,
            web_seeds: false,
            seed_ratio_limit: 3.0,
            seed_time_limit_min: 120,
            sequential: true,
            schedule_enabled: true,
            // 5000 минут — это 3 суток и 680 минут: границы заворачиваются в сутки.
            schedule_from_min: 5_000,
            schedule_to_min: 0,
            schedule_download_limit_kib: u32::MAX,
            schedule_upload_limit_kib: MAX_RATE_LIMIT_KIB + 1,
            seed_after_complete: true,
            auto_shutdown: true,
            autostart: false,
            associate_files: true,
            start_minimized: false,
            close_to_tray: false,
            notify_on_complete: true,
            watch_dir: "  C:\\Torrents\\watch  ".to_string(),
            restore_session: true,
            theme: "  LIGHT  ".to_string(),
        };
        dto.normalize();
        assert_eq!(dto.download_dir, "C:\\Torrents");
        assert_eq!(dto.watch_dir, "C:\\Torrents\\watch");
        assert_eq!(dto.max_peers, 1);
        assert_eq!(dto.max_active_downloads, MAX_ACTIVE_LIMIT);
        assert_eq!(dto.max_active_seeds, MAX_ACTIVE_LIMIT);
        assert_eq!(dto.download_limit_kib, MAX_RATE_LIMIT_KIB);
        assert_eq!(dto.upload_limit_kib, MAX_RATE_LIMIT_KIB);
        assert_eq!(dto.theme, "light");
        assert!(dto.auto_shutdown);
        assert_eq!(
            dto.proxy, "socks5://127.0.0.1:1080",
            "прокси обрезается по краям и сохраняется"
        );
        assert_eq!(dto.seed_ratio_limit, 3.0, "положительный ratio не трогаем");
        assert_eq!(
            dto.dht_port, 54_321,
            "порт DHT — любой u16, зажимать нечего"
        );
        assert_eq!(dto.seed_time_limit_min, 120);
        assert_eq!(
            dto.schedule_from_min, 680,
            "границы окна — это минуты внутри суток"
        );
        assert_eq!(dto.schedule_to_min, 0);
        assert_eq!(dto.schedule_download_limit_kib, MAX_RATE_LIMIT_KIB);
        assert_eq!(dto.schedule_upload_limit_kib, MAX_RATE_LIMIT_KIB);
        assert!(
            dto.schedule_enabled,
            "окно ненулевой длины остаётся включённым"
        );

        let mut dto = SettingsDto {
            max_peers: MAX_PEERS_LIMIT + 100,
            theme: "solarized".to_string(),
            ..SettingsDto::default()
        };
        dto.normalize();
        assert_eq!(dto.max_peers, MAX_PEERS_LIMIT);
        assert_eq!(dto.theme, DEFAULT_THEME);
    }

    #[test]
    fn empty_schedule_window_disables_schedule() {
        // Окно нулевой длины — не «круглосуточно», а «расписания нет»: иначе
        // включённый флаг молча оставлял бы пользователя без лимитов.
        let dto = SettingsDto {
            schedule_enabled: true,
            schedule_from_min: 600,
            schedule_to_min: 600,
            ..SettingsDto::default()
        }
        .normalized();
        assert!(!dto.schedule_enabled);
        assert_eq!(dto.schedule_from_min, 600);
        assert_eq!(dto.schedule_to_min, 600);

        // Асимметричное окно через полночь (22:00–06:00) остаётся включённым.
        let overnight = SettingsDto {
            schedule_enabled: true,
            schedule_from_min: 22 * 60,
            schedule_to_min: 6 * 60,
            ..SettingsDto::default()
        }
        .normalized();
        assert!(overnight.schedule_enabled);
    }

    #[test]
    fn normalize_is_idempotent() {
        let once = sample().normalized();
        let twice = once.clone().normalized();
        assert_eq!(once, twice);
    }

    #[test]
    fn unknown_theme_falls_back_to_dark() {
        assert_eq!(normalize_theme("DARK"), "dark");
        assert_eq!(normalize_theme("light"), "light");
        assert_eq!(normalize_theme(""), "dark");
        assert_eq!(normalize_theme("amoled"), "dark");
    }

    #[test]
    fn empty_download_dir_uses_fallback() {
        let fallback = Path::new("C:\\Users\\user\\Downloads");
        let dto = normalize_with_fallback(SettingsDto::default(), fallback);
        assert_eq!(dto.download_dir, "C:\\Users\\user\\Downloads");

        let dto = SettingsDto {
            download_dir: "    ".to_string(),
            ..SettingsDto::default()
        };
        let dto = normalize_with_fallback(dto, fallback);
        assert_eq!(dto.download_dir, "C:\\Users\\user\\Downloads");
    }

    #[test]
    fn prepare_creates_download_dir() {
        let dir = test_dir("prepare");
        let target = dir.join("downloads").join("nested");
        let dto = SettingsDto {
            download_dir: target.to_string_lossy().into_owned(),
            ..SettingsDto::default()
        };
        let prepared = prepare(dto, &dir).expect("каталог должен создаваться");
        assert_eq!(prepared.download_dir, target.to_string_lossy());
        assert!(target.is_dir());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_rejects_relative_dir() {
        let dir = test_dir("relative");
        let dto = SettingsDto {
            download_dir: "downloads".to_string(),
            ..SettingsDto::default()
        };
        let err = prepare(dto, &dir).expect_err("относительный путь должен отклоняться");
        assert!(
            err.contains("абсолютным"),
            "неожиданный текст ошибки: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_rejects_file_instead_of_dir() {
        let dir = test_dir("file-not-dir");
        let file_path = dir.join("not-a-dir");
        fs::write(&file_path, b"x").expect("тестовый файл должен создаваться");
        let dto = SettingsDto {
            download_dir: file_path.to_string_lossy().into_owned(),
            ..SettingsDto::default()
        };
        let err = prepare(dto, &dir).expect_err("файл вместо каталога должен отклоняться");
        assert!(
            err.contains("не каталог"),
            "неожиданный текст ошибки: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn engine_config_roundtrip() {
        let dto = sample().normalized();
        let config = to_engine_config(&dto);
        assert_eq!(config.download_dir, PathBuf::from("C:\\Torrents"));
        assert_eq!(config.max_peers_per_torrent, 80);
        assert_eq!(config.download_limit_kib, 4096);
        assert_eq!(config.upload_limit_kib, 512);
        assert_eq!(config.listen_port, 51413);
        assert!(!config.seed_after_complete);
        assert_eq!(config.max_active_downloads, 4);
        assert_eq!(config.max_active_seeds, 2);
        assert!(config.sequential, "последовательная загрузка не доехала");
        assert!(config.schedule_enabled);
        assert_eq!(config.schedule_from_min, 60);
        assert_eq!(config.schedule_to_min, 90);
        assert_eq!(config.schedule_download_limit_kib, 2048);
        assert_eq!(config.schedule_upload_limit_kib, 128);

        let back = from_engine_config(&config, &dto.theme, dto.auto_shutdown);
        // «Оконные» флаги не входят в `EngineConfig`: обратное преобразование
        // отдаёт их выключенными, настоящие значения живут только в `settings.json`.
        assert!(!back.autostart);
        assert!(!back.start_minimized);
        assert!(!back.close_to_tray);
        let expected = SettingsDto {
            autostart: false,
            start_minimized: false,
            close_to_tray: false,
            // Уведомления, наблюдение за папкой и восстановление сессии тоже не
            // входят в `EngineConfig` — обратное преобразование отдаёт значения
            // по умолчанию, а настоящие берутся из `settings.json`.
            notify_on_complete: true,
            watch_dir: String::new(),
            restore_session: true,
            ..dto
        };
        assert_eq!(back, expected);
    }

    #[test]
    fn engine_config_conversion_clamps() {
        let config = EngineConfig {
            download_dir: PathBuf::from("D:\\Torrents"),
            max_peers_per_torrent: usize::MAX,
            download_limit_kib: u32::MAX,
            upload_limit_kib: MAX_RATE_LIMIT_KIB + 1,
            listen_port: 0,
            seed_after_complete: true,
            max_active_downloads: usize::MAX,
            max_active_seeds: 5000,
            // Поля P2 к этому тесту отношения не имеют: берём значения движка.
            ..EngineConfig::default()
        };
        let dto = from_engine_config(&config, "unknown", false);
        assert_eq!(dto.max_peers, MAX_PEERS_LIMIT);
        assert_eq!(dto.max_active_downloads, MAX_ACTIVE_LIMIT);
        assert_eq!(dto.max_active_seeds, MAX_ACTIVE_LIMIT);
        assert_eq!(dto.download_limit_kib, MAX_RATE_LIMIT_KIB);
        assert_eq!(dto.upload_limit_kib, MAX_RATE_LIMIT_KIB);
        assert_eq!(dto.theme, DEFAULT_THEME);
        assert_eq!(dto.download_dir, "D:\\Torrents");
        assert!(!dto.auto_shutdown);

        // `0` = «без ограничения»: clamp не должен превращать его в 1.
        let config = EngineConfig {
            download_dir: PathBuf::from("D:\\Torrents"),
            max_peers_per_torrent: 0,
            max_active_downloads: 0,
            max_active_seeds: 0,
            ..EngineConfig::default()
        };
        let dto = from_engine_config(&config, "dark", false);
        assert_eq!(dto.max_peers, 1);
        assert_eq!(dto.max_active_downloads, 0);
        assert_eq!(dto.max_active_seeds, 0);
    }

    #[test]
    fn p2_settings_reach_engine_config() {
        // Поля P2 должны доезжать до движка, а не подменяться значениями по
        // умолчанию: иначе настройки «сохраняются», но ни на что не влияют.
        let dto = SettingsDto {
            dht: false,
            dht_port: 6881,
            pex: false,
            web_seeds: false,
            proxy: "socks5://user:pass@127.0.0.1:1080".to_string(),
            port_mapping: false,
            seed_ratio_limit: 1.5,
            seed_time_limit_min: 240,
            ..SettingsDto::default()
        }
        .normalized();

        let config = to_engine_config(&dto);
        assert!(!config.dht, "DHT не доехал до движка");
        assert_eq!(config.dht_port, 6881);
        assert!(!config.pex, "PEX не доехал до движка");
        assert!(!config.web_seeds, "web seeds не доехали до движка");
        assert_eq!(
            config.proxy.as_deref(),
            Some("socks5://user:pass@127.0.0.1:1080"),
            "прокси не доехал до движка"
        );
        assert!(!config.port_mapping, "проброс порта не доехал до движка");
        assert_eq!(config.seed_ratio_limit, 1.5);
        assert_eq!(config.seed_time_limit_min, 240);

        // Обратное преобразование возвращает те же значения, а не заглушки.
        let back = from_engine_config(&config, DEFAULT_THEME, false);
        assert!(!back.dht);
        assert_eq!(back.dht_port, 6881);
        assert!(!back.pex);
        assert!(!back.web_seeds);
        assert_eq!(back.proxy, "socks5://user:pass@127.0.0.1:1080");
        assert!(!back.port_mapping);
        assert_eq!(back.seed_ratio_limit, 1.5);
        assert_eq!(back.seed_time_limit_min, 240);

        // Пустая строка настроек — это `None` для движка (прокси выключен),
        // и обратно в настройки она возвращается пустой строкой.
        let disabled = to_engine_config(&SettingsDto::default().normalized());
        assert_eq!(disabled.proxy, None, "пустая строка — прокси выключен");
        assert_eq!(disabled.seed_ratio_limit, 0.0, "0.0 — без ограничения");
        assert_eq!(disabled.seed_time_limit_min, 0);
        assert_eq!(disabled.dht_port, 0, "0 — свободный UDP-порт для DHT");
        assert!(disabled.dht && disabled.pex && disabled.web_seeds && disabled.port_mapping);
        assert!(from_engine_config(&disabled, DEFAULT_THEME, false)
            .proxy
            .is_empty());
    }

    #[test]
    fn p3_settings_reach_engine_config() {
        // Последовательная загрузка и расписание скорости должны доезжать до
        // движка: иначе настройка «сохраняется», но ни на что не влияет.
        let dto = SettingsDto {
            sequential: true,
            schedule_enabled: true,
            schedule_from_min: 22 * 60,
            schedule_to_min: 6 * 60,
            schedule_download_limit_kib: 100,
            schedule_upload_limit_kib: 25,
            ..SettingsDto::default()
        }
        .normalized();

        let config = to_engine_config(&dto);
        assert!(config.sequential, "последовательность не доехала до движка");
        assert!(config.schedule_enabled, "расписание не доехало до движка");
        assert_eq!(config.schedule_from_min, 1320);
        assert_eq!(config.schedule_to_min, 360);
        assert_eq!(config.schedule_download_limit_kib, 100);
        assert_eq!(config.schedule_upload_limit_kib, 25);

        // Обратное преобразование возвращает те же значения, а не заглушки.
        let back = from_engine_config(&config, DEFAULT_THEME, false);
        assert!(back.sequential);
        assert!(back.schedule_enabled);
        assert_eq!(back.schedule_from_min, 1320);
        assert_eq!(back.schedule_to_min, 360);
        assert_eq!(back.schedule_download_limit_kib, 100);
        assert_eq!(back.schedule_upload_limit_kib, 25);

        // Окно нулевой длины гасит флаг ещё в настройках, поэтому до движка
        // пустое расписание не доезжает вовсе.
        let empty_window = SettingsDto {
            schedule_enabled: true,
            schedule_from_min: 100,
            schedule_to_min: 100,
            ..SettingsDto::default()
        }
        .normalized();
        assert!(!to_engine_config(&empty_window).schedule_enabled);
    }

    #[test]
    fn p3_settings_use_snake_case_keys() {
        let dir = test_dir("p3-keys");
        let path = settings_path(&dir);
        save(&path, &sample()).expect("настройки должны сохраняться");
        let text = fs::read_to_string(&path).expect("файл настроек должен читаться");
        for key in [
            "\"sequential\": true",
            "\"schedule_enabled\": true",
            "\"schedule_from_min\": 60",
            "\"schedule_to_min\": 90",
            "\"schedule_download_limit_kib\": 2048",
            "\"schedule_upload_limit_kib\": 128",
        ] {
            assert!(text.contains(key), "нет ключа {key}: {text}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_settings_without_p3_keys_normalize_to_defaults() {
        // Файл от 1.6: полей P3 в нём нет, и это не ошибка — берутся значения
        // по умолчанию (`serde(default)`), а расписание выключено.
        let dir = test_dir("legacy-p3");
        let path = settings_path(&dir);
        let legacy = r#"{
  "download_dir": "C:\\Torrents",
  "max_peers": 60,
  "theme": "dark"
}"#;
        fs::write(&path, legacy).expect("старый settings.json должен записываться");
        let loaded = load(&path, Path::new("C:\\Fallback"));
        assert!(!loaded.sequential);
        assert!(!loaded.schedule_enabled);
        assert_eq!(loaded.schedule_from_min, 0);
        assert_eq!(loaded.schedule_to_min, 0);
        assert_eq!(loaded.schedule_download_limit_kib, 0);
        assert_eq!(loaded.schedule_upload_limit_kib, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn portable_marker_switches_data_dir_to_exe_dir() {
        let exe_dir = test_dir("portable");
        let app_dir = test_dir("portable-appdata");
        fs::write(exe_dir.join(PORTABLE_MARKER), b"").expect("маркер должен создаваться");

        let resolved = resolve_data_dir(Some(&exe_dir), &app_dir);
        assert!(resolved.portable, "маркер включает портативный режим");
        assert_eq!(resolved.dir, exe_dir.join(PORTABLE_DATA_DIR_NAME));
        assert!(resolved.dir.is_dir(), "каталог данных создаётся сразу");
        let _ = fs::remove_dir_all(&exe_dir);
        let _ = fs::remove_dir_all(&app_dir);
    }

    #[test]
    fn portable_accepts_short_marker_name() {
        let exe_dir = test_dir("portable-short");
        let app_dir = test_dir("portable-short-appdata");
        fs::write(exe_dir.join(PORTABLE_MARKER_ALT), b"").expect("маркер должен создаваться");

        let resolved = resolve_data_dir(Some(&exe_dir), &app_dir);
        assert!(resolved.portable);
        assert_eq!(resolved.dir, exe_dir.join(PORTABLE_DATA_DIR_NAME));
        let _ = fs::remove_dir_all(&exe_dir);
        let _ = fs::remove_dir_all(&app_dir);
    }

    #[test]
    fn no_marker_keeps_app_data_dir() {
        let exe_dir = test_dir("portable-none");
        let app_dir = test_dir("portable-none-appdata");

        let resolved = resolve_data_dir(Some(&exe_dir), &app_dir);
        assert!(!resolved.portable);
        assert_eq!(resolved.dir, app_dir);
        // Без exe (теоретически невозможно на Windows) тоже не паникуем.
        assert_eq!(resolve_data_dir(None, &app_dir).dir, app_dir);
        let _ = fs::remove_dir_all(&exe_dir);
        let _ = fs::remove_dir_all(&app_dir);
    }

    #[test]
    fn broken_portable_dir_falls_back_to_app_data_dir() {
        // «Каталог данных» занят файлом: создать каталог нельзя, поэтому
        // приложение остаётся на `%APPDATA%` и не падает.
        let exe_dir = test_dir("portable-broken");
        let app_dir = test_dir("portable-broken-appdata");
        fs::write(exe_dir.join(PORTABLE_MARKER), b"").expect("маркер должен создаваться");
        fs::write(exe_dir.join(PORTABLE_DATA_DIR_NAME), b"not a dir")
            .expect("файл-помеха должен создаваться");

        let resolved = resolve_data_dir(Some(&exe_dir), &app_dir);
        assert!(
            !resolved.portable,
            "нерабочий каталог — не портативный режим"
        );
        assert_eq!(resolved.dir, app_dir);
        let _ = fs::remove_dir_all(&exe_dir);
        let _ = fs::remove_dir_all(&app_dir);
    }

    #[test]
    fn settings_and_session_live_in_resolved_data_dir() {
        // Одна и та же функция даёт каталог и настройкам, и сессии.
        let exe_dir = test_dir("portable-shared");
        let app_dir = test_dir("portable-shared-appdata");
        fs::write(exe_dir.join(PORTABLE_MARKER), b"").expect("маркер должен создаваться");
        let resolved = resolve_data_dir(Some(&exe_dir), &app_dir);

        assert_eq!(
            settings_path(&resolved.dir),
            resolved.dir.join(SETTINGS_FILE_NAME)
        );
        assert_eq!(
            crate::session::SessionStore::new(&resolved.dir).dir(),
            resolved.dir.join("session")
        );
        let _ = fs::remove_dir_all(&exe_dir);
        let _ = fs::remove_dir_all(&app_dir);
    }

    #[test]
    fn active_limits_use_snake_case_keys() {
        let dir = test_dir("active-limits-keys");
        let path = settings_path(&dir);
        save(&path, &sample()).expect("настройки должны сохраняться");
        let text = fs::read_to_string(&path).expect("файл настроек должен читаться");
        assert!(
            text.contains("\"max_active_downloads\": 4"),
            "нет ключа лимита загрузок: {text}"
        );
        assert!(
            text.contains("\"max_active_seeds\": 2"),
            "нет ключа лимита раздач: {text}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_active_limits_are_unlimited() {
        let dto = SettingsDto::default().normalized();
        assert_eq!(dto.max_active_downloads, 0);
        assert_eq!(dto.max_active_seeds, 0);
        let config = to_engine_config(&dto);
        assert_eq!(config.max_active_downloads, 0);
        assert_eq!(config.max_active_seeds, 0);
        assert_eq!(from_engine_config(&config, "dark", false), dto);
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = test_dir("save-load");
        let path = settings_path(&dir);
        let dto = sample();
        save(&path, &dto).expect("настройки должны сохраняться");
        assert!(path.is_file());

        let loaded = load(&path, Path::new("C:\\Fallback"));
        assert_eq!(loaded, dto);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_settings_without_window_flags_load_as_disabled() {
        let dir = test_dir("legacy-settings");
        let path = settings_path(&dir);
        // Файл от версии 1.1: новых ключей в нём ещё нет.
        let legacy = r#"{
  "download_dir": "C:\\Torrents",
  "max_peers": 60,
  "download_limit_kib": 0,
  "upload_limit_kib": 100,
  "listen_port": 51413,
  "seed_after_complete": true,
  "auto_shutdown": false,
  "theme": "dark"
}"#;
        fs::write(&path, legacy).expect("старый settings.json должен записываться");
        let loaded = load(&path, Path::new("C:\\Fallback"));
        assert!(
            !loaded.autostart,
            "автозапуск должен читаться как выключенный"
        );
        assert!(
            !loaded.start_minimized,
            "запуск в трее должен быть выключен"
        );
        assert!(
            !loaded.close_to_tray,
            "закрытие в трей должно быть выключено"
        );
        assert_eq!(loaded.max_peers, 60);
        assert_eq!(
            loaded.max_active_downloads, 0,
            "старый settings.json означает «без ограничения загрузок»"
        );
        assert_eq!(
            loaded.max_active_seeds, 0,
            "старый settings.json означает «без ограничения раздач»"
        );
        assert_eq!(loaded.upload_limit_kib, 100);
        assert!(loaded.seed_after_complete);
        assert_eq!(loaded.theme, "dark");
        assert!(
            loaded.notify_on_complete,
            "уведомления о завершении включены по умолчанию"
        );
        assert!(
            loaded.restore_session,
            "восстановление сессии включено по умолчанию"
        );
        assert!(
            loaded.watch_dir.is_empty(),
            "старый settings.json не включает папку наблюдения"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_settings_use_snake_case_keys_and_survive_roundtrip() {
        let dir = test_dir("new-keys");
        let path = settings_path(&dir);
        let dto = SettingsDto {
            // Пустой каталог загрузки при чтении заменяется запасным, поэтому
            // в ожидаемом значении он задан явно.
            download_dir: "C:\\Fallback".to_string(),
            notify_on_complete: false,
            watch_dir: "C:\\Torrents\\watch".to_string(),
            restore_session: false,
            ..SettingsDto::default()
        };
        save(&path, &dto).expect("настройки должны сохраняться");

        let text = fs::read_to_string(&path).expect("файл настроек должен читаться");
        assert!(
            text.contains("\"notify_on_complete\": false"),
            "нет ключа уведомлений: {text}"
        );
        assert!(
            text.contains("\"watch_dir\": \"C:\\\\Torrents\\\\watch\""),
            "нет ключа папки наблюдения: {text}"
        );
        assert!(
            text.contains("\"restore_session\": false"),
            "нет ключа восстановления сессии: {text}"
        );

        let loaded = load(&path, Path::new("C:\\Fallback"));
        assert_eq!(loaded, dto);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn p2_fields_default_to_enabled() {
        let dto = SettingsDto::default();
        assert!(dto.dht, "DHT включён по умолчанию");
        assert_eq!(dto.dht_port, 0, "порт DHT по умолчанию — любой свободный");
        assert!(dto.pex, "PEX включён по умолчанию");
        assert!(dto.proxy.is_empty(), "прокси по умолчанию выключен");
        assert!(dto.port_mapping, "проброс порта включён по умолчанию");
        assert!(dto.web_seeds, "web seeds включены по умолчанию");
        assert_eq!(
            dto.seed_ratio_limit, 0.0,
            "лимит ratio по умолчанию выключен"
        );
        assert_eq!(
            dto.seed_time_limit_min, 0,
            "лимит времени раздачи по умолчанию выключен"
        );
    }

    #[test]
    fn p2_fields_use_snake_case_keys_and_survive_roundtrip() {
        let dir = test_dir("p2-keys");
        let path = settings_path(&dir);
        let dto = SettingsDto {
            download_dir: "C:\\Fallback".to_string(),
            dht: false,
            dht_port: 6881,
            pex: false,
            proxy: "socks5://user:pass@127.0.0.1:1080".to_string(),
            port_mapping: false,
            web_seeds: false,
            seed_ratio_limit: 2.5,
            seed_time_limit_min: 120,
            ..SettingsDto::default()
        };
        save(&path, &dto).expect("настройки должны сохраняться");

        let text = fs::read_to_string(&path).expect("файл настроек должен читаться");
        for expected in [
            "\"dht\": false",
            "\"dht_port\": 6881",
            "\"pex\": false",
            "\"port_mapping\": false",
            "\"web_seeds\": false",
            "\"seed_ratio_limit\": 2.5",
            "\"seed_time_limit_min\": 120",
        ] {
            assert!(text.contains(expected), "нет ключа {expected}: {text}");
        }
        assert!(
            text.contains("\"proxy\": \"socks5://user:pass@127.0.0.1:1080\""),
            "нет ключа прокси: {text}"
        );

        let loaded = load(&path, Path::new("C:\\Fallback"));
        assert_eq!(loaded, dto, "поля P2 должны пережить сохранение и чтение");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalize_seed_ratio_drops_negative_and_nan() {
        // Недопустимые значения превращаются в «без ограничения», а не ломают
        // остальные настройки.
        assert_eq!(normalize_seed_ratio(-1.5), 0.0);
        assert_eq!(normalize_seed_ratio(f64::NAN), 0.0);
        assert_eq!(normalize_seed_ratio(f64::INFINITY), 0.0);
        assert_eq!(normalize_seed_ratio(f64::NEG_INFINITY), 0.0);
        assert_eq!(normalize_seed_ratio(-0.0), 0.0);
        // Положительное значение — законный лимит ratio.
        assert_eq!(normalize_seed_ratio(1.5), 1.5);

        let mut dto = SettingsDto {
            seed_ratio_limit: -3.0,
            ..SettingsDto::default()
        };
        dto.normalize();
        assert_eq!(dto.seed_ratio_limit, 0.0, "отрицательный ratio обнуляется");

        let mut dto = SettingsDto {
            seed_ratio_limit: f64::NAN,
            ..SettingsDto::default()
        };
        dto.normalize();
        assert_eq!(dto.seed_ratio_limit, 0.0, "NaN не должен попасть в файл");
        assert_eq!(
            dto.clone().normalized(),
            dto,
            "после исправления нормализация идемпотентна"
        );
    }

    #[test]
    fn normalize_proxy_keeps_valid_and_drops_broken() {
        // Валидные строки сохраняются (по краям обрезаются). Правила — те же,
        // что у движка: `torrent_core::proxy::ProxyConfig::parse`.
        for spec in [
            "socks5://127.0.0.1:1080",
            "  socks5://127.0.0.1:1080  ",
            "http://proxy.local:3128",
            "https://proxy.local:8443",
            "SOCKS5://user:pass@127.0.0.1:1080",
            "socks5://user:pass@127.0.0.1:1080",
            "socks5h://127.0.0.1:1080",
            "socks5://[::1]:1080",
            // Порт можно опустить: движок подставит 1080 для SOCKS5 и 8080 для
            // HTTP, поэтому такие строки не считаем опечаткой.
            "socks5://127.0.0.1",
            "http://proxy.local",
            // Путь движок игнорирует (см. `ProxyConfig::parse`), так что строка
            // не ломается даже если пользователь скопировал её из браузера.
            "http://proxy.local:3128/path",
            // Логин без пароля движок принимает — не придумываем правил строже.
            "socks5://user@127.0.0.1:1080",
        ] {
            assert_eq!(
                normalize_proxy(spec),
                spec.trim(),
                "прокси «{spec}» должен сохраниться"
            );
        }

        // Опечатка не должна оставлять пользователя без сети: строка
        // сбрасывается в пустую (прокси выключен).
        for spec in [
            "127.0.0.1:1080",
            "socks4://127.0.0.1:1080",
            "proxy.local:3128",
            "socks5://",
            "socks5://127.0.0.1:0",
            "socks5://127.0.0.1:port",
            "socks5://127.0.0.1:99999",
            "socks5://:1080",
            "socks5://::1:1080",
            "socks5://127.0.0.1:1080 с пробелом",
        ] {
            assert!(
                normalize_proxy(spec).is_empty(),
                "прокси «{spec}» должен отбрасываться"
            );
        }

        assert!(normalize_proxy("").is_empty(), "пустая строка — выключено");
        assert!(
            normalize_proxy("   ").is_empty(),
            "пробелы — тоже выключено"
        );

        // Нормализация полного DTO отбрасывает негодный прокси, но сохраняет
        // остальные настройки нетронутыми.
        let mut dto = SettingsDto {
            proxy: "socks4://127.0.0.1:1080".to_string(),
            max_peers: 77,
            ..SettingsDto::default()
        };
        dto.normalize();
        assert!(dto.proxy.is_empty());
        assert_eq!(dto.max_peers, 77);
    }

    #[test]
    fn serialized_settings_have_exactly_the_contract_keys() {
        // Контракт с фронтендом: JSON — плоский объект с snake_case-ключами и
        // без «лишних» полей (лишний ключ — повод для несовпадения версий).
        let value = serde_json::to_value(SettingsDto::default())
            .expect("настройки должны сериализоваться в JSON");
        let object = value.as_object().expect("настройки — JSON-объект");
        let mut actual: Vec<&str> = object.keys().map(String::as_str).collect();
        actual.sort_unstable();

        let mut expected = vec![
            "auto_shutdown",
            "autostart",
            "associate_files",
            "close_to_tray",
            "dht",
            "dht_port",
            "download_dir",
            "download_limit_kib",
            "listen_port",
            "max_active_downloads",
            "max_active_seeds",
            "max_peers",
            "notify_on_complete",
            "pex",
            "port_mapping",
            "proxy",
            "restore_session",
            "schedule_download_limit_kib",
            "schedule_enabled",
            "schedule_from_min",
            "schedule_to_min",
            "schedule_upload_limit_kib",
            "seed_after_complete",
            "seed_ratio_limit",
            "seed_time_limit_min",
            "sequential",
            "start_minimized",
            "theme",
            "upload_limit_kib",
            "watch_dir",
            "web_seeds",
        ];
        expected.sort_unstable();

        assert_eq!(actual, expected, "набор ключей разошёлся с контрактом");
        for (key, value) in object {
            assert!(
                key.chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'),
                "ключ «{key}» не в snake_case"
            );
            assert!(!value.is_null(), "поле «{key}» не должно быть null");
        }
    }

    #[test]
    fn old_settings_without_p2_fields_get_p2_defaults() {
        let dir = test_dir("p2-legacy");
        let path = settings_path(&dir);
        // `settings.json` версии 1.5: полей P2 в нём ещё нет.
        let legacy = r#"{
  "download_dir": "C:\\Torrents",
  "max_peers": 60,
  "listen_port": 51413,
  "seed_after_complete": true,
  "theme": "dark"
}"#;
        fs::write(&path, legacy).expect("старый settings.json должен записываться");

        let loaded = load(&path, Path::new("C:\\Fallback"));
        assert!(loaded.dht, "DHT включается по умолчанию");
        assert_eq!(loaded.dht_port, 0);
        assert!(loaded.pex, "PEX включается по умолчанию");
        assert!(loaded.proxy.is_empty(), "прокси выключен");
        assert!(loaded.port_mapping, "проброс порта включён по умолчанию");
        assert!(loaded.web_seeds, "web seeds включены по умолчанию");
        // Новое поле 1.7: старый `settings.json` его не содержит, а привязка
        // `.torrent` должна включиться сама — иначе баг с ассоциацией не уйдёт.
        assert!(
            loaded.associate_files,
            "привязка .torrent включена по умолчанию"
        );
        assert_eq!(loaded.seed_ratio_limit, 0.0);
        assert_eq!(loaded.seed_time_limit_min, 0);
        // Остальные поля читаются как раньше.
        assert_eq!(loaded.max_peers, 60);
        assert_eq!(loaded.listen_port, 51413);
        assert_eq!(loaded.theme, "dark");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_watch_dir_allows_empty_and_creates_missing() {
        // Пустая строка — наблюдение выключено.
        prepare_watch_dir("").expect("пустой путь допустим");
        prepare_watch_dir("   ").expect("пробелы — тоже пустой путь");

        let dir = test_dir("watch-create");
        let target = dir.join("watch").join("nested");
        // Каталога ещё нет: `prepare` должен его создать.
        prepare_watch_dir(&target.to_string_lossy()).expect("папка наблюдения создаётся");
        assert!(target.is_dir());
        // Повторный вызов на существующем каталоге не ошибка.
        prepare_watch_dir(&target.to_string_lossy()).expect("существующая папка допустима");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_watch_dir_rejects_relative_and_files() {
        let err = prepare_watch_dir("watch").expect_err("относительный путь отклоняется");
        assert!(err.contains("абсолютным"), "текст ошибки: {err}");

        let dir = test_dir("watch-file");
        let file_path = dir.join("not-a-dir");
        fs::write(&file_path, b"x").expect("файл должен создаваться");
        let err = prepare_watch_dir(&file_path.to_string_lossy())
            .expect_err("файл вместо папки отклоняется");
        assert!(err.contains("не каталог"), "текст ошибки: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_rejects_relative_watch_dir_but_keeps_download_dir() {
        let dir = test_dir("watch-prepare");
        let download = dir.join("downloads");
        let dto = SettingsDto {
            download_dir: download.to_string_lossy().into_owned(),
            watch_dir: "relative".to_string(),
            ..SettingsDto::default()
        };
        let err = prepare(dto, &dir).expect_err("относительная папка наблюдения отклоняется");
        assert!(err.contains("папка наблюдения"), "текст ошибки: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn window_flags_survive_normalize_and_save() {
        let dir = test_dir("window-flags");
        let path = settings_path(&dir);
        let dto = SettingsDto {
            autostart: true,
            start_minimized: true,
            close_to_tray: true,
            ..SettingsDto::default()
        };
        let normalized = dto.clone().normalized();
        assert_eq!(
            normalized, dto,
            "нормализация не должна менять «оконные» флаги"
        );

        save(&path, &normalized).expect("настройки должны сохраняться");
        let text = fs::read_to_string(&path).expect("файл настроек должен читаться");
        assert!(
            text.contains("\"autostart\": true"),
            "нет ключа автозапуска: {text}"
        );
        assert!(
            text.contains("\"start_minimized\": true"),
            "нет ключа запуска в трее: {text}"
        );
        assert!(
            text.contains("\"close_to_tray\": true"),
            "нет ключа закрытия в трей: {text}"
        );

        let loaded = load(&path, Path::new("C:\\Fallback"));
        assert!(loaded.autostart && loaded.start_minimized && loaded.close_to_tray);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let dir = test_dir("atomic");
        let path = settings_path(&dir);
        save(&path, &sample()).expect("первое сохранение");
        save(&path, &SettingsDto::default()).expect("второе сохранение");

        let leftovers: Vec<_> = fs::read_dir(&dir)
            .expect("каталог должен читаться")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "остались временные файлы: {leftovers:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let dir = test_dir("missing");
        let path = settings_path(&dir);
        let loaded = load(&path, Path::new("C:\\Fallback"));
        assert_eq!(loaded.download_dir, "C:\\Fallback");
        assert_eq!(loaded.max_peers, DEFAULT_MAX_PEERS);
        assert_eq!(loaded.theme, DEFAULT_THEME);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_broken_json_returns_defaults() {
        let dir = test_dir("broken");
        let path = settings_path(&dir);
        fs::write(&path, b"{ not json at all ").expect("битый файл должен создаваться");
        let loaded = load(&path, Path::new("C:\\Fallback"));
        assert_eq!(loaded.download_dir, "C:\\Fallback");
        assert_eq!(loaded.max_peers, DEFAULT_MAX_PEERS);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_partial_json_fills_defaults() {
        let dir = test_dir("partial");
        let path = settings_path(&dir);
        fs::write(&path, br#"{"download_dir":"C:\\Other","max_peers":7}"#)
            .expect("частичный JSON должен создаваться");
        let loaded = load(&path, Path::new("C:\\Fallback"));
        assert_eq!(loaded.download_dir, "C:\\Other");
        assert_eq!(loaded.max_peers, 7);
        assert_eq!(loaded.theme, DEFAULT_THEME);
        assert_eq!(loaded.download_limit_kib, 0);
        assert!(
            !loaded.auto_shutdown,
            "старый settings.json не включает автовыключение"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_migrates_legacy_peer_default() {
        // Так выглядит settings.json клиента 1.7.0: 50 — это старое умолчание,
        // а не осознанный выбор, поэтому после обновления поднимаем до 200.
        let dir = test_dir("legacy-peers");
        let path = settings_path(&dir);
        fs::write(&path, br#"{"download_dir":"C:\\Other","max_peers":50}"#)
            .expect("старый JSON должен создаваться");
        let loaded = load(&path, Path::new("C:\\Fallback"));
        assert_eq!(loaded.max_peers, DEFAULT_MAX_PEERS);
        assert_eq!(
            loaded.download_dir, "C:\\Other",
            "остальные поля не тронуты"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_keeps_explicit_peer_limit() {
        // Не 50 — значит пользователь выбрал сам, миграция не должна вмешиваться.
        let dir = test_dir("explicit-peers");
        let path = settings_path(&dir);
        fs::write(&path, br#"{"download_dir":"C:\\Other","max_peers":128}"#)
            .expect("JSON с явным лимитом должен создаваться");
        let loaded = load(&path, Path::new("C:\\Fallback"));
        assert_eq!(loaded.max_peers, 128);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_reports_error_for_unwritable_path() {
        // Каталог в качестве родителя файла настроек: запись невозможна.
        let dir = test_dir("unwritable");
        let path = dir.join("busy").join(SETTINGS_FILE_NAME);
        fs::create_dir_all(&path).expect("каталог на месте файла создаётся");
        let err = save(&path, &sample()).expect_err("запись в каталог должна падать");
        assert!(!err.is_empty());
        assert!(err.starts_with("не удалось"), "неожиданный текст: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_path_uses_app_dir() {
        let path = settings_path(Path::new(
            "C:\\Users\\user\\AppData\\Roaming\\com.torrenta.client",
        ));
        assert!(path.ends_with(SETTINGS_FILE_NAME));
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some(SETTINGS_FILE_NAME)
        );
    }
}
