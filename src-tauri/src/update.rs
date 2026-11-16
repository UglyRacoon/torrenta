//! Автоматическое обновление через GitHub Releases.
//!
//! Плагин `tauri-plugin-updater` требует подписанного манифеста и ключа, а у
//! проекта его нет. Поэтому канал обновлений простой и прозрачный: приложение
//! спрашивает у GitHub последний релиз, сравнивает версии и — по согласию
//! пользователя — скачивает установщик из этого же релиза, запускает его в
//! тихом режиме и перезапускается.
//!
//! Что здесь есть и чего нет:
//! * проверка идёт только к [`RELEASES_URL`] (api.github.com, репозиторий проекта);
//! * установщик скачивается только с `github.com` (поле `browser_download_url`);
//! * криптографической подписи нет: сборки не подписаны сертификатом, и
//!   проверить целостность можно лишь косвенно (HTTPS + размер файла из API).
//!   Это ограничение записано в документации, а не спрятано.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Событие «появилось обновление» (payload — [`UpdateStateDto`]).
pub const UPDATE_AVAILABLE_EVENT: &str = "engine://update-available";
/// Событие прогресса скачивания установщика (payload — [`UpdateProgressDto`]).
pub const UPDATE_PROGRESS_EVENT: &str = "engine://update-progress";

/// Репозиторий проекта.
pub const REPO: &str = "UglyRacoon/torrenta";
/// Адрес последнего релиза в GitHub API.
pub const RELEASES_URL: &str = "https://api.github.com/repos/UglyRacoon/torrenta/releases/latest";
/// `User-Agent` обязателен для GitHub API.
const USER_AGENT: &str = "Torrenta-Updater";
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// Сколько символов описания релиза отдаём интерфейсу.
const NOTES_LIMIT: usize = 1200;

/// Один файл релиза.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub size: u64,
    pub url: String,
}

/// Разобранный релиз GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Версия без ведущей `v` (`1.9.0`).
    pub version: String,
    /// Тег как он есть (`v1.9.0`).
    pub tag: String,
    /// Страница релиза в браузере.
    pub page_url: String,
    /// Описание релиза (обрезанное).
    pub notes: String,
    pub assets: Vec<ReleaseAsset>,
}

impl Release {
    /// Установщик для Windows: сначала NSIS (`*-setup.exe`), потом MSI.
    pub fn windows_asset(&self) -> Option<&ReleaseAsset> {
        self.assets
            .iter()
            .find(|asset| asset.name.to_ascii_lowercase().ends_with("-setup.exe"))
            .or_else(|| {
                self.assets
                    .iter()
                    .find(|asset| asset.name.to_ascii_lowercase().ends_with(".msi"))
            })
    }
}

/// Состояние проверки обновлений для интерфейса.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UpdateStateDto {
    /// Текущая версия приложения.
    pub current_version: String,
    /// Версия доступного обновления (`None` — новее нет).
    pub available: Option<String>,
    /// Пояснение человеческим языком.
    pub message: Option<String>,
    /// Страница релиза.
    pub page_url: Option<String>,
    /// Прямая ссылка на установщик.
    pub asset_url: Option<String>,
    /// Размер установщика в байтах (0 — неизвестен).
    pub asset_size: u64,
    /// Краткое описание релиза.
    pub notes: Option<String>,
    /// Канал проверки доступен (у нас он всегда доступен: GitHub Releases).
    pub configured: bool,
}

/// Прогресс скачивания установщика.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct UpdateProgressDto {
    /// Сколько байт уже получено.
    pub received: u64,
    /// Ожидаемый размер (0 — неизвестен).
    pub total: u64,
    /// Процент 0..=100; `None`, если размер неизвестен.
    pub percent: Option<f64>,
}

/// Строит HTTP-клиент для проверки и скачивания.
pub fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(CHECK_TIMEOUT)
        .build()
        .map_err(|err| format!("не удалось создать HTTP-клиент для обновлений: {err}"))
}

/// Сравнивает версии: `true`, если `candidate` новее `current`.
///
/// Сравниваются числовые компоненты (`1.9.0` > `1.8.10`), суффиксы вроде
/// `-beta` считаются «младше» той же числовой версии без суффикса. Мусор в
/// строке не ломает функцию: нечисловые части считаются нулями.
pub fn version_is_newer(current: &str, candidate: &str) -> bool {
    fn parts(text: &str) -> (Vec<u64>, bool) {
        let trimmed = text.trim().trim_start_matches(['v', 'V']);
        // Отделяем числовую часть от суффикса: `1.9.0-beta.1` → (`1.9.0`, true).
        let (numeric, suffix) = match trimmed.find('-') {
            Some(index) => (&trimmed[..index], &trimmed[index + 1..]),
            None => (trimmed, ""),
        };
        let numbers: Vec<u64> = numeric
            .split('.')
            .map(|part| {
                let digits: String = part.chars().take_while(|ch| ch.is_ascii_digit()).collect();
                digits.parse::<u64>().unwrap_or(0)
            })
            .collect();
        (numbers, !suffix.is_empty())
    }

    let (current_numbers, current_pre) = parts(current);
    let (candidate_numbers, candidate_pre) = parts(candidate);
    let width = current_numbers.len().max(candidate_numbers.len()).max(3);
    for index in 0..width {
        let left = current_numbers.get(index).copied().unwrap_or(0);
        let right = candidate_numbers.get(index).copied().unwrap_or(0);
        if right != left {
            return right > left;
        }
    }
    // Числа равны: релиз-кандидат без суффикса не новее, чем суффиксный.
    current_pre && !candidate_pre
}

/// Разбирает JSON ответа GitHub API.
pub fn parse_release(json: &str) -> Result<Release, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|err| format!("GitHub вернул некорректный JSON: {err}"))?;
    let tag = value
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "в ответе GitHub нет поля tag_name".to_string())?
        .trim()
        .to_string();
    if tag.is_empty() {
        return Err("в ответе GitHub пустой tag_name".to_string());
    }
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    let page_url = value
        .get("html_url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let notes = value
        .get("body")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(NOTES_LIMIT)
        .collect::<String>();
    let mut assets = Vec::new();
    if let Some(list) = value.get("assets").and_then(serde_json::Value::as_array) {
        for item in list {
            let Some(name) = item.get("name").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(url) = item
                .get("browser_download_url")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            // Скачиваем только с GitHub: чужой домен в ответе API игнорируем.
            if !url.starts_with("https://github.com/") {
                continue;
            }
            assets.push(ReleaseAsset {
                name: name.to_string(),
                size: item
                    .get("size")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                url: url.to_string(),
            });
        }
    }
    Ok(Release {
        version,
        tag,
        page_url,
        notes,
        assets,
    })
}

/// Проверяет наличие новой версии. `Ok(None)` — новее нет.
pub async fn check(
    client: &reqwest::Client,
    current_version: &str,
) -> Result<Option<Release>, String> {
    let response = client
        .get(RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|err| format!("не удалось проверить обновления: {err}"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        // Релизов ещё нет — это не ошибка сети.
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(format!(
            "GitHub ответил {} при проверке обновлений",
            response.status().as_u16()
        ));
    }
    let text = response
        .text()
        .await
        .map_err(|err| format!("не удалось прочитать ответ GitHub: {err}"))?;
    let release = parse_release(&text)?;
    if version_is_newer(current_version, &release.version) {
        Ok(Some(release))
    } else {
        Ok(None)
    }
}

/// Превращает релиз в DTO для интерфейса.
pub fn state_for(current_version: &str, release: Option<&Release>) -> UpdateStateDto {
    match release {
        Some(release) => {
            let asset = release.windows_asset();
            UpdateStateDto {
                current_version: current_version.to_string(),
                available: Some(release.version.clone()),
                message: Some(format!(
                    "доступна версия {} — можно скачать и установить",
                    release.version
                )),
                page_url: Some(release.page_url.clone()),
                asset_url: asset.map(|asset| asset.url.clone()),
                asset_size: asset.map(|asset| asset.size).unwrap_or(0),
                notes: Some(release.notes.clone()),
                configured: true,
            }
        }
        None => UpdateStateDto {
            current_version: current_version.to_string(),
            available: None,
            message: Some("установлена последняя версия".to_string()),
            page_url: None,
            asset_url: None,
            asset_size: 0,
            notes: None,
            configured: true,
        },
    }
}

// ---------------------------------------------------------------------------
// Кэш последней проверки
// ---------------------------------------------------------------------------

fn cache() -> &'static Mutex<Option<Release>> {
    static CACHE: OnceLock<Mutex<Option<Release>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Запоминает найденный релиз, чтобы кнопка «Установить» не проверяла заново.
pub fn remember(release: Option<Release>) {
    if let Ok(mut slot) = cache().lock() {
        *slot = release;
    }
}

/// Забирает последний найденный релиз из кэша.
pub fn cached() -> Option<Release> {
    cache().lock().ok().and_then(|slot| slot.clone())
}

// ---------------------------------------------------------------------------
// Скачивание и установка
// ---------------------------------------------------------------------------

/// Каталог для скачанного установщика.
pub fn download_dir() -> PathBuf {
    std::env::temp_dir().join("TorrentaUpdate")
}

/// Скачивает установщик, сообщая прогресс.
///
/// Файл пишется во временный каталог и удаляется при ошибке, чтобы не оставлять
/// недокачанный установщик, который потом можно запустить вручную.
pub async fn download<F>(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    mut on_progress: F,
) -> Result<(), String>
where
    F: FnMut(u64, u64),
{
    if !url.starts_with("https://github.com/") {
        return Err("ссылка на установщик ведёт не на GitHub".to_string());
    }
    let response = client
        .get(url)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .map_err(|err| format!("не удалось скачать установщик: {err}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "GitHub ответил {} при скачивании установщика",
            response.status().as_u16()
        ));
    }
    let total = response.content_length().unwrap_or(0);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("не удалось создать каталог обновления: {err}"))?;
    }
    let mut file = fs::File::create(destination)
        .map_err(|err| format!("не удалось создать файл установщика: {err}"))?;
    let mut response = response;
    let mut received: u64 = 0;
    loop {
        let chunk = match response.chunk().await {
            Ok(chunk) => chunk,
            Err(err) => {
                drop(file);
                let _ = fs::remove_file(destination);
                return Err(format!("соединение прервалось при скачивании: {err}"));
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        if let Err(err) = file.write_all(&chunk) {
            drop(file);
            let _ = fs::remove_file(destination);
            return Err(format!("не удалось записать установщик: {err}"));
        }
        received += chunk.len() as u64;
        on_progress(received, total);
    }
    file.flush()
        .map_err(|err| format!("не удалось дописать установщик: {err}"))?;
    drop(file);
    if total > 0 && received != total {
        let _ = fs::remove_file(destination);
        return Err(format!(
            "установщик скачан не полностью: {received} из {total} байт"
        ));
    }
    Ok(())
}

/// Запускает установку и перезапуск приложения.
///
/// Приложение нельзя обновить «на живую»: установщик перезаписывает собственный
/// exe. Поэтому пишем во временный каталог `.bat`, который ждёт выхода текущего
/// процесса, запускает установщик в тихом режиме (`/S` для NSIS, `/qn` для MSI)
/// и снова открывает Torrenta. Сам процесс после запуска скрипта должен выйти.
#[cfg(windows)]
pub fn install_and_restart(installer: &Path, exe: &Path, current_pid: u32) -> Result<(), String> {
    use std::os::windows::process::CommandExt as _;
    /// `CREATE_NO_WINDOW`: окна консоли у скрипта быть не должно.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let dir = download_dir();
    fs::create_dir_all(&dir)
        .map_err(|err| format!("не удалось создать каталог обновления: {err}"))?;
    let script = dir.join("apply-update.bat");
    let is_msi = installer
        .extension()
        .map(|ext| ext.eq_ignore_ascii_case("msi"))
        .unwrap_or(false);
    let run = if is_msi {
        format!(
            "start \"\" /wait msiexec /i \"{}\" /qn",
            installer.display()
        )
    } else {
        format!("start \"\" /wait \"{}\" /S", installer.display())
    };
    let content = format!(
        "@echo off\r\n\
         chcp 65001 >nul\r\n\
         :wait\r\n\
         tasklist /FI \"PID eq {pid}\" | find \"{pid}\" >nul\r\n\
         if not errorlevel 1 (\r\n\
         \x20 timeout /t 1 /nobreak >nul\r\n\
         \x20 goto wait\r\n\
         )\r\n\
         {run}\r\n\
         start \"\" \"{exe}\"\r\n\
         del \"%~f0\"\r\n",
        pid = current_pid,
        exe = exe.display()
    );
    fs::write(&script, content)
        .map_err(|err| format!("не удалось подготовить установку обновления: {err}"))?;
    std::process::Command::new("cmd")
        .arg("/C")
        .arg(&script)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|err| format!("не удалось запустить установщик: {err}"))?;
    Ok(())
}

/// Не-Windows: обновление установщиком не поддерживается.
#[cfg(not(windows))]
pub fn install_and_restart(_installer: &Path, _exe: &Path, _pid: u32) -> Result<(), String> {
    Err("автоматическое обновление поддерживается только в Windows-сборке".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_json(tag: &str, assets: &[(&str, u64)]) -> String {
        let list: Vec<String> = assets
            .iter()
            .map(|(name, size)| {
                format!(
                    r#"{{"name":"{name}","size":{size},"browser_download_url":"https://github.com/UglyRacoon/torrenta/releases/download/{tag}/{name}"}}"#
                )
            })
            .collect();
        format!(
            r#"{{"tag_name":"{tag}","html_url":"https://github.com/UglyRacoon/torrenta/releases/tag/{tag}","body":"Описание релиза","assets":[{}]}}"#,
            list.join(",")
        )
    }

    #[test]
    fn version_compare_handles_numbers_and_suffixes() {
        assert!(version_is_newer("1.8.1", "1.9.0"));
        assert!(version_is_newer("1.8.10", "1.8.11"));
        assert!(!version_is_newer("1.9.0", "1.8.99"));
        assert!(!version_is_newer("1.9.0", "v1.9.0"));
        assert!(version_is_newer("1.9.0", "1.9.1"));
        // Пре-релиз не считается обновлением над релизом той же версии…
        assert!(!version_is_newer("1.9.0", "1.9.0-beta.1"));
        // …но бета новее предыдущей версии.
        assert!(version_is_newer("1.8.1", "1.9.0-beta.1"));
        // Мусор не ломает сравнение.
        assert!(!version_is_newer("1.9.0", "не-версия"));
    }

    #[test]
    fn parse_release_reads_tag_assets_and_notes() {
        let json = release_json(
            "v1.9.0",
            &[
                ("Torrenta_1.9.0_x64-setup.exe", 5_000_000),
                ("Torrenta_1.9.0_x64_ru-RU.msi", 6_800_000),
            ],
        );
        let release = parse_release(&json).expect("валидный JSON должен разбираться");
        assert_eq!(release.version, "1.9.0");
        assert_eq!(release.tag, "v1.9.0");
        assert_eq!(release.notes, "Описание релиза");
        assert!(release.page_url.contains("releases/tag/v1.9.0"));
        assert_eq!(release.assets.len(), 2);
        let asset = release.windows_asset().expect("установщик должен найтись");
        assert_eq!(asset.name, "Torrenta_1.9.0_x64-setup.exe");
        assert_eq!(asset.size, 5_000_000);
    }

    #[test]
    fn parse_release_prefers_setup_and_ignores_foreign_domains() {
        let json = r#"{
            "tag_name": "v1.9.1",
            "html_url": "https://github.com/UglyRacoon/torrenta/releases/tag/v1.9.1",
            "assets": [
                {"name": "Torrenta_1.9.1_x64_ru-RU.msi", "size": 5, "browser_download_url": "https://github.com/UglyRacoon/torrenta/releases/download/v1.9.1/a.msi"},
                {"name": "Torrenta_1.9.1_x64-setup.exe", "size": 7, "browser_download_url": "https://github.com/UglyRacoon/torrenta/releases/download/v1.9.1/b.exe"},
                {"name": "evil.exe", "size": 9, "browser_download_url": "https://evil.example.com/evil.exe"}
            ]
        }"#;
        let release = parse_release(json).expect("разбор без ошибок");
        assert_eq!(release.assets.len(), 2, "чужой домен отбрасывается");
        assert_eq!(
            release.windows_asset().map(|asset| asset.name.as_str()),
            Some("Torrenta_1.9.1_x64-setup.exe")
        );
    }

    #[test]
    fn parse_release_requires_tag() {
        assert!(parse_release(r#"{"assets":[]}"#).is_err());
        assert!(parse_release(r#"{"tag_name":""}"#).is_err());
        assert!(parse_release("не json").is_err());
    }

    #[test]
    fn state_reports_availability() {
        let release = parse_release(&release_json(
            "v1.9.0",
            &[("Torrenta_1.9.0_x64-setup.exe", 42)],
        ))
        .expect("разбор");
        let state = state_for("1.8.1", Some(&release));
        assert_eq!(state.available.as_deref(), Some("1.9.0"));
        assert_eq!(state.asset_size, 42);
        assert!(state.configured);
        assert!(state.notes.is_some());

        let none = state_for("1.9.0", None);
        assert!(none.available.is_none());
        assert!(none.message.unwrap_or_default().contains("последняя"));
    }

    #[test]
    fn progress_percent_is_optional() {
        let progress = UpdateProgressDto {
            received: 50,
            total: 200,
            percent: Some(25.0),
        };
        assert_eq!(progress.percent, Some(25.0));
        let unknown = UpdateProgressDto {
            received: 50,
            total: 0,
            percent: None,
        };
        assert!(unknown.percent.is_none());
    }
}
