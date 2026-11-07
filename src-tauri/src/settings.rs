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
pub const DEFAULT_MAX_PEERS: u32 = 50;
/// Верхняя граница числа пиров на торрент.
pub const MAX_PEERS_LIMIT: u32 = 500;
/// Верхняя граница лимита скорости — 1 ГиБ/с (в КиБ/с).
pub const MAX_RATE_LIMIT_KIB: u32 = 1_048_576;
/// Верхняя граница listen-порта: `0` означает «выбрать свободный».
pub const MAX_LISTEN_PORT: u16 = u16::MAX;
/// Допустимые темы оформления.
pub const THEMES: [&str; 2] = ["dark", "light"];
/// Тема оформления по умолчанию.
pub const DEFAULT_THEME: &str = "dark";

/// Настройки в том виде, в котором их видит фронтенд.
///
/// Все поля в `snake_case` — это контракт с фронтендом, договорённый в
/// `docs/PRODUCT.md` / задаче; переименовывать их без согласования нельзя.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct SettingsDto {
    /// Каталог, куда сохраняются данные торрентов. Абсолютный путь.
    pub download_dir: String,
    /// Максимальное число пиров на торрент (1..=500).
    pub max_peers: u32,
    /// Ограничение скорости загрузки в КиБ/с, `0` — без лимита.
    pub download_limit_kib: u32,
    /// Ограничение скорости отдачи в КиБ/с, `0` — без лимита.
    pub upload_limit_kib: u32,
    /// Порт для входящих подключений, `0` — свободный порт.
    pub listen_port: u16,
    /// Раздавать после завершения загрузки.
    pub seed_after_complete: bool,
    /// Тема оформления: `"dark"` или `"light"`.
    pub theme: String,
}

impl Default for SettingsDto {
    fn default() -> Self {
        Self {
            download_dir: String::new(),
            max_peers: DEFAULT_MAX_PEERS,
            download_limit_kib: 0,
            upload_limit_kib: 0,
            listen_port: 0,
            seed_after_complete: true,
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
        self.max_peers = clamp_peers(self.max_peers);
        self.download_limit_kib = clamp_rate(self.download_limit_kib);
        self.upload_limit_kib = clamp_rate(self.upload_limit_kib);
        self.theme = normalize_theme(&self.theme);
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

/// Приводит название темы к одному из [`THEMES`]; всё неизвестное — `dark`.
pub fn normalize_theme(raw: &str) -> String {
    let lowered = raw.trim().to_ascii_lowercase();
    if THEMES.contains(&lowered.as_str()) {
        lowered
    } else {
        DEFAULT_THEME.to_string()
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

    Ok(dto)
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
    }
}

/// Обратное преобразование: конфигурация движка → DTO настроек.
///
/// Тема не входит в `EngineConfig`, поэтому передаётся отдельно.
pub fn from_engine_config(config: &EngineConfig, theme: &str) -> SettingsDto {
    SettingsDto {
        download_dir: config.download_dir.to_string_lossy().into_owned(),
        max_peers: u32::try_from(config.max_peers_per_torrent)
            .unwrap_or(MAX_PEERS_LIMIT)
            .clamp(1, MAX_PEERS_LIMIT),
        download_limit_kib: clamp_rate(config.download_limit_kib),
        upload_limit_kib: clamp_rate(config.upload_limit_kib),
        listen_port: config.listen_port,
        seed_after_complete: config.seed_after_complete,
        theme: normalize_theme(theme),
    }
}

/// Путь к файлу настроек внутри каталога конфигурации приложения.
pub fn settings_path(app_dir: &Path) -> PathBuf {
    app_dir.join(SETTINGS_FILE_NAME)
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

    normalize_with_fallback(stored.unwrap_or_default(), fallback_dir)
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
            download_limit_kib: 4096,
            upload_limit_kib: 512,
            listen_port: 51413,
            seed_after_complete: false,
            theme: "light".to_string(),
        }
    }

    #[test]
    fn defaults_are_sane() {
        let dto = SettingsDto::default();
        assert_eq!(dto.max_peers, DEFAULT_MAX_PEERS);
        assert_eq!(dto.download_limit_kib, 0);
        assert_eq!(dto.upload_limit_kib, 0);
        assert_eq!(dto.listen_port, 0);
        assert!(dto.seed_after_complete);
        assert_eq!(dto.theme, DEFAULT_THEME);
        assert!(dto.download_dir.is_empty());
        assert_eq!(dto, dto.clone().normalized());
    }

    #[test]
    fn normalize_clamps_out_of_range_values() {
        let mut dto = SettingsDto {
            download_dir: "   C:\\Torrents   ".to_string(),
            max_peers: 0,
            download_limit_kib: u32::MAX,
            upload_limit_kib: MAX_RATE_LIMIT_KIB + 1,
            listen_port: 65_535,
            seed_after_complete: true,
            theme: "  LIGHT  ".to_string(),
        };
        dto.normalize();
        assert_eq!(dto.download_dir, "C:\\Torrents");
        assert_eq!(dto.max_peers, 1);
        assert_eq!(dto.download_limit_kib, MAX_RATE_LIMIT_KIB);
        assert_eq!(dto.upload_limit_kib, MAX_RATE_LIMIT_KIB);
        assert_eq!(dto.theme, "light");

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
        assert!(err.contains("абсолютным"), "неожиданный текст ошибки: {err}");
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
        assert!(err.contains("не каталог"), "неожиданный текст ошибки: {err}");
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

        let back = from_engine_config(&config, &dto.theme);
        assert_eq!(back, dto);
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
        };
        let dto = from_engine_config(&config, "unknown");
        assert_eq!(dto.max_peers, MAX_PEERS_LIMIT);
        assert_eq!(dto.download_limit_kib, MAX_RATE_LIMIT_KIB);
        assert_eq!(dto.upload_limit_kib, MAX_RATE_LIMIT_KIB);
        assert_eq!(dto.theme, DEFAULT_THEME);
        assert_eq!(dto.download_dir, "D:\\Torrents");

        let config = EngineConfig {
            download_dir: PathBuf::from("D:\\Torrents"),
            max_peers_per_torrent: 0,
            ..EngineConfig::default()
        };
        assert_eq!(from_engine_config(&config, "dark").max_peers, 1);
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
        assert!(leftovers.is_empty(), "остались временные файлы: {leftovers:?}");
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
        let path = settings_path(Path::new("C:\\Users\\user\\AppData\\Roaming\\com.torrenta.client"));
        assert!(path.ends_with(SETTINGS_FILE_NAME));
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some(SETTINGS_FILE_NAME));
    }
}
