//! Сохранение и восстановление сессии торрентов.
//!
//! Задача модуля — чтобы после перезапуска приложения список торрентов выглядел
//! как до закрытия: те же magnet-ссылки, те же выбранные файлы, те же паузы,
//! лимиты и приоритеты.
//!
//! Где лежит сессия (каталог данных приложения + [`SESSION_DIR_NAME`]).
//! Каталог данных выбирает [`crate::settings::resolve_data_dir`]: обычно это
//! `%APPDATA%\Torrenta`, в портативном режиме — `<каталог exe>\TorrentaData`.
//! Внутри:
//! * `session/session.json` — список [`SessionEntry`];
//! * `session/session-data/<info_hash>.torrent` — копия содержимого `.torrent`
//!   для торрентов, добавленных файлом: при восстановлении метаданные берутся
//!   из неё, а не из исходного файла (он мог быть удалён или переименован);
//! * `session/session-data/<info_hash>.torrent` для magnet не нужен: ссылка
//!   хранится в самом `session.json`, а метаданные при восстановлении
//!   подтягиваются у пиров заново (ограничение, см. [`restore_from_session`]).
//!
//! Запись `session.json` атомарна (временный файл + `rename`), поэтому падение
//! приложения посреди сохранения не оставляет обрезанный JSON.
//!
//! Модуль разделён на две части:
//! * [`SessionStore`] и свободные функции — работа с диском и чистые
//!   преобразования (их покрывают тесты);
//! * [`SessionRegistry`] — состояние, разделяемое с командами и тикером:
//!   помнит, откуда взялся каждый торрент (файл или magnet), потому что из
//!   снимка движка этого не видно.
//! * [`restore_from_session`] — восстановление при старте.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::Hasher as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use torrent_core::{AddOptions, FilePriority, TorrentSnapshot, TorrentState};

use crate::state::AppEngine;

/// Каталог сессии внутри каталога конфигурации приложения.
pub const SESSION_DIR_NAME: &str = "session";
/// Имя файла со списком торрентов сессии.
pub const SESSION_FILE_NAME: &str = "session.json";
/// Подкаталог с копиями `.torrent`-файлов.
pub const SESSION_DATA_DIR_NAME: &str = "session-data";
/// Расширение копий `.torrent`-файлов.
pub const SESSION_DATA_EXT: &str = "torrent";

/// Длина info-hash в hex-виде.
const INFO_HASH_HEX_LEN: usize = 40;

/// Откуда торрент попал в список — от этого зависит способ восстановления.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionSource {
    /// Торрент добавлен из `.torrent`-файла.
    ///
    /// `file` — имя копии внутри `session-data` (для диагностики; при чтении
    /// имя всегда вычисляется из `info_hash`, а не берётся из JSON).
    File {
        /// Имя файла копии метаданных в `session-data`.
        file: String,
    },
    /// Торрент добавлен по magnet-ссылке; сама ссылка и есть источник.
    Magnet {
        /// Исходная magnet-ссылка.
        link: String,
    },
}

/// Приоритет одного файла в сессии (пишем только отличия от обычного).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFilePriority {
    /// Индекс файла внутри торрента.
    pub index: usize,
    /// Приоритет загрузки файла.
    pub priority: FilePriority,
}

/// Одна запись сессии — всё, что нужно, чтобы вернуть торрент после перезапуска.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEntry {
    /// Info-hash в hex (строчные символы, 40 штук).
    pub info_hash: String,
    /// Отображаемое имя торрента (для журнала и подсказок).
    pub name: String,
    /// Источник: `.torrent`-файл или magnet-ссылка.
    pub source: SessionSource,
    /// Каталог, в который сохраняются данные торрента.
    pub save_path: String,
    /// Был ли торрент на паузе.
    #[serde(default)]
    pub paused: bool,
    /// Выбранные файлы; пустой список — «все файлы».
    ///
    /// Ограничение: торрент, у которого сняты все галочки, восстановится как
    /// «все файлы» — движок не позволяет оставить торрент без выбранных файлов,
    /// поэтому такое состояние и не встречается на практике.
    #[serde(default)]
    pub selected_files: Vec<usize>,
    /// Необычные приоритеты файлов (`Skip`, `Low`, `High`); `Normal` не пишется.
    #[serde(default)]
    pub priorities: Vec<SessionFilePriority>,
    /// Локальный лимит скачивания, КиБ/с (`0` — без ограничения).
    #[serde(default)]
    pub down_limit_kib: u32,
    /// Локальный лимит отдачи, КиБ/с (`0` — без ограничения).
    #[serde(default)]
    pub up_limit_kib: u32,
}

/// Содержимое `session.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    /// Торренты в порядке добавления.
    #[serde(default)]
    pub torrents: Vec<SessionEntry>,
}

/// Итог восстановления сессии (для журнала и тестов).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// Сколько торрентов удалось вернуть.
    pub restored: usize,
    /// Сколько записей пришлось пропустить.
    pub skipped: usize,
}

/// Что нужно сделать с одной записью сессии.
#[derive(Debug, Clone, PartialEq)]
pub enum RestoreAction {
    /// Добавить торрент из копии `.torrent`-файла.
    File {
        /// Запись сессии.
        entry: SessionEntry,
        /// Содержимое `.torrent`.
        data: Vec<u8>,
    },
    /// Добавить торрент по magnet-ссылке.
    Magnet {
        /// Запись сессии.
        entry: SessionEntry,
        /// Magnet-ссылка.
        link: String,
    },
    /// Запись бесполезна — восстановить её нельзя.
    Skip {
        /// Info-hash для журнала.
        info_hash: String,
        /// Почему запись пропущена (по-русски).
        reason: String,
    },
}

impl RestoreAction {
    /// Info-hash записи — для журнала.
    pub fn info_hash(&self) -> &str {
        match self {
            RestoreAction::File { entry, .. } | RestoreAction::Magnet { entry, .. } => {
                &entry.info_hash
            }
            RestoreAction::Skip { info_hash, .. } => info_hash,
        }
    }
}

// ---------------------------------------------------------------------------
// Чистые преобразования
// ---------------------------------------------------------------------------

/// Имя копии метаданных для info-hash (`<hash>.torrent`).
///
/// Возвращает `None`, если строка не похожа на hex-инфохеш: подставлять
/// произвольный текст в имя файла нельзя — иначе через `session.json` можно
/// было бы выйти за пределы каталога сессии.
pub fn torrent_data_name(info_hash: &str) -> Option<String> {
    let hash = info_hash.trim().to_ascii_lowercase();
    if hash.len() == INFO_HASH_HEX_LEN && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(format!("{hash}.{SESSION_DATA_EXT}"))
    } else {
        None
    }
}

/// Разбирает текст `session.json`.
///
/// Отсутствующие необязательные поля заполняются значениями по умолчанию — так
/// сессия, записанная более старой версией приложения, остаётся читаемой.
pub fn parse_session_json(text: &str) -> Result<Vec<SessionEntry>, String> {
    let state: SessionState =
        serde_json::from_str(text).map_err(|err| format!("некорректный JSON сессии: {err}"))?;
    Ok(state.torrents)
}

/// Хеш полезной нагрузки сессии: по нему тикер решает, надо ли писать файл.
///
/// Сравнение хешей дешевле сравнения самих записей и не зависит от порядка
/// полей, потому что список перед сериализацией уже упорядочен.
pub fn payload_hash(entries: &[SessionEntry]) -> u64 {
    let mut hasher = DefaultHasher::new();
    match serde_json::to_vec(entries) {
        Ok(bytes) => hasher.write(&bytes),
        // Сериализация записей не может упасть на практике (только строки и
        // числа), но и паника здесь недопустима: пусть будет «пустой» хеш.
        Err(_) => hasher.write_u8(0),
    }
    hasher.finish()
}

/// Превращает записи сессии в список действий восстановления.
///
/// Ошибки не прерывают обработку: сломанная запись попадает в
/// [`RestoreAction::Skip`] с причиной, остальные остаются пригодными.
pub fn restore_actions(entries: Vec<SessionEntry>, data_dir: &Path) -> Vec<RestoreAction> {
    entries
        .into_iter()
        .map(|entry| {
            // Источник копируем заранее: в ветке magnet-ссылки запись
            // перемещается в действие, а заимствование из неё не дало бы
            // этого сделать.
            match entry.source.clone() {
                SessionSource::File { .. } => match torrent_data_name(&entry.info_hash) {
                    Some(name) => match fs::read(data_dir.join(&name)) {
                        Ok(data) if !data.is_empty() => RestoreAction::File { entry, data },
                        Ok(_) => RestoreAction::Skip {
                            info_hash: entry.info_hash.clone(),
                            reason: format!("копия метаданных «{name}» пустая"),
                        },
                        Err(err) => RestoreAction::Skip {
                            info_hash: entry.info_hash.clone(),
                            reason: format!(
                                "не удалось прочитать копию метаданных «{name}»: {err}"
                            ),
                        },
                    },
                    None => RestoreAction::Skip {
                        info_hash: entry.info_hash.clone(),
                        reason: "info-hash в сессии повреждён".to_string(),
                    },
                },
                SessionSource::Magnet { link } => {
                    let link = link.trim().to_string();
                    if link.is_empty() {
                        RestoreAction::Skip {
                            info_hash: entry.info_hash.clone(),
                            reason: "magnet-ссылка в сессии пустая".to_string(),
                        }
                    } else {
                        RestoreAction::Magnet { entry, link }
                    }
                }
            }
        })
        .collect()
}

/// Каталог сохранения из записи сессии.
///
/// Относительный путь отклоняется (в сессии его быть не должно): движок писал
/// бы данные в рабочий каталог процесса, который пользователь не выбирал.
fn restore_save_path(entry: &SessionEntry) -> Option<PathBuf> {
    let raw = entry.save_path.trim();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        eprintln!(
            "[torrenta] сессия: каталог «{raw}» не абсолютный — использую каталог загрузок по умолчанию"
        );
        return None;
    }
    Some(path)
}

/// Выбор файлов для добавления (`None` — все файлы).
fn restore_files(entry: &SessionEntry) -> Option<Vec<usize>> {
    if entry.selected_files.is_empty() {
        return None;
    }
    let mut files = entry.selected_files.clone();
    files.sort_unstable();
    files.dedup();
    Some(files)
}

// ---------------------------------------------------------------------------
// Хранилище на диске
// ---------------------------------------------------------------------------

/// Работа с каталогом сессии: чтение, атомарная запись, копии `.torrent`.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    /// Хранилище в каталоге конфигурации приложения.
    pub fn new(app_dir: &Path) -> Self {
        SessionStore {
            dir: app_dir.join(SESSION_DIR_NAME),
        }
    }

    /// Каталог сессии.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Путь к `session.json`.
    pub fn file_path(&self) -> PathBuf {
        self.dir.join(SESSION_FILE_NAME)
    }

    /// Каталог с копиями `.torrent`.
    pub fn data_dir(&self) -> PathBuf {
        self.dir.join(SESSION_DATA_DIR_NAME)
    }

    /// Путь к копии метаданных по info-hash (`None`, если hash некорректен).
    pub fn data_path(&self, info_hash: &str) -> Option<PathBuf> {
        torrent_data_name(info_hash).map(|name| self.data_dir().join(name))
    }

    /// Читает `session.json`; отсутствие файла — пустая сессия.
    pub fn load(&self) -> Result<Vec<SessionEntry>, String> {
        let path = self.file_path();
        match fs::read_to_string(&path) {
            Ok(text) => parse_session_json(&text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(format!(
                "не удалось прочитать сессию «{}»: {err}",
                path.display()
            )),
        }
    }

    /// Атомарно записывает `session.json`.
    pub fn save(&self, entries: &[SessionEntry]) -> Result<(), String> {
        fs::create_dir_all(&self.dir).map_err(|err| {
            format!(
                "не удалось создать каталог сессии «{}»: {err}",
                self.dir.display()
            )
        })?;

        let state = SessionState {
            torrents: entries.to_vec(),
        };
        let json = serde_json::to_string_pretty(&state)
            .map_err(|err| format!("не удалось сериализовать сессию: {err}"))?;

        let path = self.file_path();
        let tmp_path = temporary_path(&path);
        {
            let mut file = fs::File::create(&tmp_path).map_err(|err| {
                format!(
                    "не удалось создать временный файл сессии «{}»: {err}",
                    tmp_path.display()
                )
            })?;
            file.write_all(json.as_bytes())
                .map_err(|err| format!("не удалось записать сессию: {err}"))?;
            file.write_all(b"\n")
                .map_err(|err| format!("не удалось записать сессию: {err}"))?;
            file.sync_all()
                .map_err(|err| format!("не удалось сохранить сессию на диск: {err}"))?;
        }

        fs::rename(&tmp_path, &path).map_err(|err| {
            let _ = fs::remove_file(&tmp_path);
            format!("не удалось сохранить сессию в «{}»: {err}", path.display())
        })
    }

    /// Кладёт копию `.torrent` в `session-data`.
    pub fn write_torrent(&self, info_hash: &str, data: &[u8]) -> Result<(), String> {
        let path = self
            .data_path(info_hash)
            .ok_or_else(|| format!("некорректный info-hash «{info_hash}»"))?;
        let dir = self.data_dir();
        fs::create_dir_all(&dir).map_err(|err| {
            format!(
                "не удалось создать каталог данных сессии «{}»: {err}",
                dir.display()
            )
        })?;

        let tmp_path = temporary_path(&path);
        {
            let mut file = fs::File::create(&tmp_path).map_err(|err| {
                format!(
                    "не удалось создать временный файл «{}»: {err}",
                    tmp_path.display()
                )
            })?;
            file.write_all(data)
                .map_err(|err| format!("не удалось записать копию .torrent: {err}"))?;
            file.sync_all()
                .map_err(|err| format!("не удалось сохранить копию .torrent: {err}"))?;
        }
        fs::rename(&tmp_path, &path).map_err(|err| {
            let _ = fs::remove_file(&tmp_path);
            format!(
                "не удалось сохранить копию .torrent в «{}»: {err}",
                path.display()
            )
        })
    }

    /// Удаляет копию метаданных (нет файла — не ошибка).
    pub fn remove_torrent(&self, info_hash: &str) -> Result<(), String> {
        let Some(path) = self.data_path(info_hash) else {
            return Ok(());
        };
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(format!(
                "не удалось удалить копию .torrent «{}»: {err}",
                path.display()
            )),
        }
    }

    /// Есть ли на диске копия метаданных для info-hash.
    pub fn has_torrent(&self, info_hash: &str) -> bool {
        self.data_path(info_hash)
            .map(|path| path.is_file())
            .unwrap_or(false)
    }
}

/// Путь временного файла для атомарной записи (в том же каталоге).
fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_string());
    path.with_file_name(format!(".{name}.tmp"))
}

// ---------------------------------------------------------------------------
// Разделяемое состояние
// ---------------------------------------------------------------------------

/// Источники торрентов, добавленных в текущем запуске.
///
/// Из снимка движка видно info-hash и каталог, но не видно, чем торрент был
/// добавлен — файлом или ссылкой. Реестр помнит это и умеет собирать записи
/// сессии из снимка состояния.
#[derive(Debug)]
pub struct SessionRegistry {
    /// Карта «id торрента в движке → источник».
    origins: RwLock<HashMap<u64, SessionSource>>,
    /// Файлы сессии.
    store: SessionStore,
}

impl SessionRegistry {
    /// Создаёт реестр для каталога конфигурации приложения.
    pub fn new(app_dir: &Path) -> Self {
        SessionRegistry {
            origins: RwLock::new(HashMap::new()),
            store: SessionStore::new(app_dir),
        }
    }

    /// Хранилище на диске (каталоги, `session.json`).
    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    /// Каталог сессии.
    pub fn dir(&self) -> &Path {
        self.store.dir()
    }

    /// Карта источников; `Err` от отравленного лока не пробрасывается — данные
    /// остаются согласованными, паниковать из-за этого нельзя.
    fn origins_read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<u64, SessionSource>> {
        match self.origins.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn origins_write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<u64, SessionSource>> {
        match self.origins.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Запоминает, что торрент добавлен из `.torrent`-файла, и кладёт копию
    /// метаданных рядом с сессией.
    pub fn register_file(&self, id: u64, info_hash: &str, data: &[u8]) -> Result<(), String> {
        self.store.write_torrent(info_hash, data)?;
        let name = torrent_data_name(info_hash)
            .ok_or_else(|| format!("некорректный info-hash «{info_hash}»"))?;
        self.origins_write()
            .insert(id, SessionSource::File { file: name });
        Ok(())
    }

    /// Запоминает, что торрент добавлен по magnet-ссылке.
    pub fn register_magnet(&self, id: u64, link: &str) -> Result<(), String> {
        let link = link.trim();
        if link.is_empty() {
            return Err("пустая magnet-ссылка".to_string());
        }
        self.origins_write().insert(
            id,
            SessionSource::Magnet {
                link: link.to_string(),
            },
        );
        Ok(())
    }

    /// Забывает торрент (он удалён из списка).
    ///
    /// Заодно удаляет копию `.torrent` из `session-data`: иначе она осталась бы
    /// на диске навсегда и занимала место. Ошибка удаления не мешает забыть
    /// запись — файл может быть занят другим процессом.
    pub fn forget(&self, id: u64) -> Option<SessionSource> {
        let source = self.origins_write().remove(&id)?;
        if let SessionSource::File { file } = &source {
            if let Some(stem) = Path::new(file).file_stem().and_then(|stem| stem.to_str()) {
                if let Err(err) = self.store.remove_torrent(stem) {
                    eprintln!("[torrenta] не удалось удалить копию торрента из сессии: {err}");
                }
            }
        }
        Some(source)
    }

    /// Источник торрента, если он известен.
    pub fn origin(&self, id: u64) -> Option<SessionSource> {
        self.origins_read().get(&id).cloned()
    }

    /// Сколько торрентов помнит реестр (для диагностики).
    pub fn len(&self) -> usize {
        self.origins_read().len()
    }

    /// Пуст ли реестр.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Собирает записи сессии из снимка состояния движка.
    ///
    /// Порядок записей детерминированный (по info-hash), чтобы хеш полезной
    /// нагрузки не менялся от перестановки торрентов в списке.
    pub fn entries(&self, torrents: &[TorrentSnapshot]) -> Vec<SessionEntry> {
        let mut entries: Vec<SessionEntry> = torrents
            .iter()
            .filter_map(|torrent| self.entry_for(torrent))
            .collect();
        entries.sort_by(|left, right| {
            left.info_hash
                .cmp(&right.info_hash)
                .then_with(|| left.name.cmp(&right.name))
        });
        entries
    }

    /// Одна запись сессии; `None` — торрент восстановить не получится.
    fn entry_for(&self, torrent: &TorrentSnapshot) -> Option<SessionEntry> {
        let source = match self.origin(torrent.id) {
            Some(source) => source,
            None => {
                // Источник потерялся (например, копию не удалось записать при
                // добавлении): если копия `.torrent` на диске уже есть, торрент
                // всё равно можно вернуть — считаем его файловым.
                let file = torrent_data_name(&torrent.info_hash)?;
                if !self.store.has_torrent(&torrent.info_hash) {
                    return None;
                }
                SessionSource::File { file }
            }
        };

        let mut selected_files = Vec::new();
        let mut priorities = Vec::new();
        for (index, file) in torrent.files.iter().enumerate() {
            if file.selected {
                selected_files.push(index);
            }
            if file.priority != FilePriority::Normal {
                priorities.push(SessionFilePriority {
                    index,
                    priority: file.priority,
                });
            }
        }
        // Пустой список означает «все файлы», поэтому полный выбор не пишем.
        if selected_files.len() == torrent.files.len() {
            selected_files.clear();
        }

        Some(SessionEntry {
            info_hash: torrent.info_hash.clone(),
            name: torrent.name.clone(),
            source,
            save_path: torrent.save_path.clone(),
            paused: torrent.state == TorrentState::Paused,
            selected_files,
            priorities,
            down_limit_kib: torrent.down_limit_kib,
            up_limit_kib: torrent.up_limit_kib,
        })
    }

    /// Сохраняет записи в `session.json`.
    pub fn save(&self, entries: &[SessionEntry]) -> Result<(), String> {
        self.store.save(entries)
    }

    /// Читает `session.json`.
    pub fn load(&self) -> Result<Vec<SessionEntry>, String> {
        self.store.load()
    }
}

// ---------------------------------------------------------------------------
// Восстановление
// ---------------------------------------------------------------------------

/// Восстанавливает торренты из сессии.
///
/// Порядок важен: сначала торренты появляются в списке, затем к каждому
/// применяются пауза, лимиты и приоритеты. Ошибка на одном торренте не мешает
/// остальным — она печатается в stderr один раз, а цикл продолжается.
///
/// Ограничение magnet-ссылок: метаданные при восстановлении качаются заново
/// (в сессии лежит только ссылка). Пауза и лимиты применяются сразу, а
/// приоритеты файлов — только если метаданные успели прийти; у magnet-торрентов
/// восстанавливается выбор файлов, но не приоритеты `Low`/`High`.
pub async fn restore_from_session(engine: &AppEngine, registry: &SessionRegistry) -> RestoreReport {
    let mut report = RestoreReport::default();

    let entries = match registry.load() {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("[torrenta] сессия не восстановлена: {err}");
            return report;
        }
    };
    if entries.is_empty() {
        return report;
    }

    let actions = restore_actions(entries, &registry.store().data_dir());
    for action in actions {
        match apply_restore_action(engine, registry, action).await {
            Ok(()) => report.restored += 1,
            Err(err) => {
                report.skipped += 1;
                eprintln!("[torrenta] сессия: {err}");
            }
        }
    }

    if report.restored > 0 || report.skipped > 0 {
        eprintln!(
            "[torrenta] сессия восстановлена: {} торрентов, пропущено {}",
            report.restored, report.skipped
        );
    }
    report
}

/// Выполняет одно действие восстановления.
async fn apply_restore_action(
    engine: &AppEngine,
    registry: &SessionRegistry,
    action: RestoreAction,
) -> Result<(), String> {
    match action {
        RestoreAction::Skip { info_hash, reason } => {
            Err(format!("торрент {info_hash} пропущен: {reason}"))
        }
        RestoreAction::File { entry, data } => {
            let opts = AddOptions {
                save_path: restore_save_path(&entry),
                files: restore_files(&entry),
            };
            let id = engine
                .manager
                .add_file_with(data.clone(), opts)
                .await
                .map_err(|err| format!("не удалось вернуть «{}» ({err})", entry.name.trim()))?;

            if let Err(err) = registry.register_file(id, &entry.info_hash, &data) {
                eprintln!(
                    "[torrenta] сессия: не удалось запомнить источник «{}»: {err}",
                    entry.name
                );
            }
            apply_entry_details(engine, id, &entry);
            Ok(())
        }
        RestoreAction::Magnet { entry, link } => {
            let opts = AddOptions {
                save_path: restore_save_path(&entry),
                files: restore_files(&entry),
            };
            let id = engine
                .manager
                .add_magnet_with(&link, opts)
                .await
                .map_err(|err| {
                    format!(
                        "не удалось вернуть «{}» по magnet-ссылке ({err})",
                        entry.name.trim()
                    )
                })?;

            if let Err(err) = registry.register_magnet(id, &link) {
                eprintln!(
                    "[torrenta] сессия: не удалось запомнить источник «{}»: {err}",
                    entry.name
                );
            }
            apply_entry_details(engine, id, &entry);
            Ok(())
        }
    }
}

/// Применяет к добавленному торренту паузу, лимиты и приоритеты из записи.
///
/// Ошибки только логируются: торрент уже в списке, и терять его из-за
/// неприменённого лимита нельзя.
fn apply_entry_details(engine: &AppEngine, id: u64, entry: &SessionEntry) {
    if entry.paused {
        if let Err(err) = engine.manager.pause(id) {
            eprintln!(
                "[torrenta] сессия: не удалось поставить «{}» на паузу: {err}",
                entry.name.trim()
            );
        }
    }

    if entry.down_limit_kib > 0 || entry.up_limit_kib > 0 {
        if let Err(err) = engine
            .manager
            .set_limits(id, entry.down_limit_kib, entry.up_limit_kib)
        {
            eprintln!(
                "[torrenta] сессия: не удалось вернуть лимиты «{}»: {err}",
                entry.name.trim()
            );
        }
    }

    if entry.priorities.is_empty() {
        return;
    }

    // Длину списка приоритетов задаёт число файлов, а его знает только движок.
    let file_count = engine
        .manager
        .snapshot()
        .into_iter()
        .find(|torrent| torrent.id == id)
        .map(|torrent| torrent.files.len())
        .unwrap_or(0);
    if file_count == 0 {
        // У magnet-торрента метаданные ещё не пришли: приоритеты применить не
        // к чему, а движок не принимает список другой длины.
        eprintln!(
            "[torrenta] сессия: у «{}» пока нет метаданных — приоритеты файлов не восстановлены",
            entry.name.trim()
        );
        return;
    }

    let mut priorities = vec![FilePriority::Normal; file_count];
    for item in &entry.priorities {
        if let Some(slot) = priorities.get_mut(item.index) {
            *slot = item.priority;
        }
    }
    if let Err(err) = engine.manager.set_file_priorities(id, priorities) {
        eprintln!(
            "[torrenta] сессия: не удалось вернуть приоритеты «{}»: {err}",
            entry.name.trim()
        );
    }
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use torrent_core::engine::PeerView;
    use torrent_core::FileView;

    /// Временный каталог с уникальным именем (без внешних зависимостей).
    fn test_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "torrenta-session-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("временный каталог должен создаваться");
        dir
    }

    fn hash(ch: char) -> String {
        std::iter::repeat_n(ch, INFO_HASH_HEX_LEN).collect()
    }

    fn entry(info_hash: &str) -> SessionEntry {
        SessionEntry {
            info_hash: info_hash.to_string(),
            name: "Тестовый торрент".to_string(),
            source: SessionSource::File {
                file: torrent_data_name(info_hash).unwrap_or_default(),
            },
            save_path: "C:\\Загрузки".to_string(),
            paused: false,
            selected_files: Vec::new(),
            priorities: Vec::new(),
            down_limit_kib: 0,
            up_limit_kib: 0,
        }
    }

    fn magnet_entry(info_hash: &str, link: &str) -> SessionEntry {
        SessionEntry {
            source: SessionSource::Magnet {
                link: link.to_string(),
            },
            ..entry(info_hash)
        }
    }

    fn file_view(path: &str, selected: bool, priority: FilePriority) -> FileView {
        FileView {
            path: path.to_string(),
            length: 1024,
            downloaded: 0,
            selected,
            priority,
        }
    }

    fn snapshot(id: u64, info_hash: &str, state: TorrentState) -> TorrentSnapshot {
        TorrentSnapshot {
            id,
            name: "Тестовый торрент".to_string(),
            info_hash: info_hash.to_string(),
            state,
            error: None,
            total_bytes: 2048,
            wanted_bytes: 2048,
            remaining_bytes: 2048,
            swarm_seeds: 0,
            swarm_leechers: 0,
            downloaded_bytes: 0,
            uploaded_bytes: 0,
            progress: 0.0,
            piece_count: 2,
            piece_length: 16384,
            pieces_have: 0,
            peers_connected: 0,
            peers_known: 0,
            down_rate: 0.0,
            up_rate: 0.0,
            down_limit_kib: 0,
            up_limit_kib: 0,
            eta_seconds: None,
            // P2: счётчик ratio и признак лимита раздачи к тесту отношения не
            // имеют — берём значения по умолчанию.
            seed_ratio: 0.0,
            seed_limit_reached: false,
            save_path: "C:\\Загрузки".to_string(),
            added_at: 0,
            files: vec![
                file_view("a.bin", true, FilePriority::Normal),
                file_view("b.bin", true, FilePriority::Normal),
            ],
            trackers: Vec::new(),
            peers: Vec::<PeerView>::new(),
            // P3: порядок скачивания к тесту сессии отношения не имеет.
            sequential: false,
        }
    }

    #[test]
    fn session_roundtrip_keeps_every_field() {
        let dir = test_dir("roundtrip");
        let store = SessionStore::new(&dir);

        let mut first = entry(&hash('a'));
        first.paused = true;
        first.selected_files = vec![0, 2];
        first.priorities = vec![
            SessionFilePriority {
                index: 0,
                priority: FilePriority::High,
            },
            SessionFilePriority {
                index: 2,
                priority: FilePriority::Skip,
            },
        ];
        first.down_limit_kib = 1024;
        first.up_limit_kib = 128;
        let second = magnet_entry(&hash('b'), "magnet:?xt=urn:btih:abc");

        let entries = vec![first, second];
        store.save(&entries).expect("сессия должна сохраняться");

        let loaded = store.load().expect("сессия должна читаться");
        assert_eq!(loaded, entries);
    }

    #[test]
    fn partial_json_fills_defaults() {
        let text = format!(
            r#"{{"torrents":[{{"info_hash":"{}","name":"Старый","source":{{"kind":"file","file":"x.torrent"}},"save_path":"C:\\Загрузки"}}]}}"#,
            hash('c')
        );
        let entries = parse_session_json(&text).expect("частичный JSON должен читаться");
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert!(!entry.paused);
        assert!(entry.selected_files.is_empty());
        assert!(entry.priorities.is_empty());
        assert_eq!(entry.down_limit_kib, 0);
        assert_eq!(entry.up_limit_kib, 0);
    }

    #[test]
    fn empty_json_object_is_empty_session() {
        let entries = parse_session_json("{}").expect("пустой объект — валидная сессия");
        assert!(entries.is_empty());
    }

    #[test]
    fn broken_json_is_an_error_not_a_panic() {
        let err = parse_session_json("{это не json").expect_err("битый JSON должен быть ошибкой");
        assert!(err.contains("некорректный JSON сессии"), "текст: {err}");
    }

    #[test]
    fn load_missing_file_returns_empty_session() {
        let dir = test_dir("missing");
        let store = SessionStore::new(&dir);
        assert!(store.load().expect("нет файла — пустая сессия").is_empty());
    }

    #[test]
    fn load_broken_file_reports_error() {
        let dir = test_dir("broken");
        let store = SessionStore::new(&dir);
        std::fs::create_dir_all(store.dir()).expect("каталог сессии");
        std::fs::write(store.file_path(), "{битый").expect("файл сессии");
        assert!(store.load().is_err());
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let dir = test_dir("atomic");
        let store = SessionStore::new(&dir);
        store
            .save(&[entry(&hash('d'))])
            .expect("сессия должна сохраняться");

        let leftovers: Vec<String> = std::fs::read_dir(store.dir())
            .expect("каталог сессии должен существовать")
            .filter_map(|item| item.ok())
            .map(|item| item.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "остались временные файлы: {leftovers:?}"
        );
        assert_eq!(store.load().expect("сессия читается").len(), 1);
    }

    #[test]
    fn torrent_copy_is_written_and_removed() {
        let dir = test_dir("copy");
        let store = SessionStore::new(&dir);
        let info_hash = hash('e');

        store
            .write_torrent(&info_hash, b"d4:infod4:name1:ae")
            .expect("копия должна сохраняться");
        assert!(store.has_torrent(&info_hash));

        let path = store.data_path(&info_hash).expect("путь к копии");
        assert!(path.is_file(), "копия должна лежать в session-data");

        store.remove_torrent(&info_hash).expect("копия удаляется");
        assert!(!store.has_torrent(&info_hash));
        // Повторное удаление не ошибка.
        store
            .remove_torrent(&info_hash)
            .expect("повторное удаление");
    }

    #[test]
    fn torrent_data_name_rejects_traversal() {
        assert!(torrent_data_name("../../etc/passwd").is_none());
        assert!(torrent_data_name("короткий").is_none());
        assert!(
            torrent_data_name(&hash('F')).is_some(),
            "верхний регистр допустим"
        );
        assert_eq!(
            torrent_data_name(&hash('a')).as_deref(),
            Some(format!("{}.torrent", hash('a')).as_str())
        );
    }

    #[test]
    fn restore_actions_skip_broken_entries_only() {
        let dir = test_dir("actions");
        let data_dir = dir.join(SESSION_DATA_DIR_NAME);
        std::fs::create_dir_all(&data_dir).expect("каталог данных");

        let file_hash = hash('1');
        std::fs::write(
            data_dir.join(torrent_data_name(&file_hash).expect("имя копии")),
            b"d4:infod4:name1:ae",
        )
        .expect("копия .torrent");

        let empty_hash = hash('2');
        std::fs::write(
            data_dir.join(torrent_data_name(&empty_hash).expect("имя копии")),
            b"",
        )
        .expect("пустая копия");

        let missing_hash = hash('3');
        let broken_hash = "не-хеш";
        let magnet_hash = hash('4');

        let entries = vec![
            entry(&file_hash),
            entry(&empty_hash),
            entry(&missing_hash),
            SessionEntry {
                info_hash: broken_hash.to_string(),
                ..entry(&file_hash)
            },
            magnet_entry(&magnet_hash, "  magnet:?xt=urn:btih:aa  "),
            magnet_entry(&hash('5'), "   "),
        ];

        let actions = restore_actions(entries, &data_dir);
        assert_eq!(actions.len(), 6, "ни одна запись не должна потеряться");

        assert!(matches!(actions[0], RestoreAction::File { .. }));
        let RestoreAction::Skip { reason, .. } = &actions[1] else {
            panic!("пустая копия должна пропускаться");
        };
        assert!(reason.contains("пустая"), "текст: {reason}");

        let RestoreAction::Skip { reason, .. } = &actions[2] else {
            panic!("отсутствующая копия должна пропускаться");
        };
        assert!(reason.contains("не удалось прочитать"), "текст: {reason}");

        let RestoreAction::Skip { reason, .. } = &actions[3] else {
            panic!("кривой info-hash должен пропускаться");
        };
        assert!(reason.contains("info-hash"), "текст: {reason}");

        let RestoreAction::Magnet { link, .. } = &actions[4] else {
            panic!("magnet должен восстанавливаться");
        };
        assert_eq!(link, "magnet:?xt=urn:btih:aa", "ссылка должна обрезаться");

        assert!(matches!(actions[5], RestoreAction::Skip { .. }));
    }

    #[test]
    fn entries_are_built_from_snapshot() {
        let dir = test_dir("entries");
        let registry = SessionRegistry::new(&dir);
        let info_hash = hash('6');
        registry
            .register_file(1, &info_hash, b"d4:infod4:name1:ae")
            .expect("источник должен запоминаться");

        let mut torrent = snapshot(1, &info_hash, TorrentState::Paused);
        torrent.files = vec![
            file_view("a.bin", true, FilePriority::Normal),
            file_view("b.bin", false, FilePriority::Skip),
            file_view("c.bin", true, FilePriority::High),
        ];
        torrent.down_limit_kib = 512;
        torrent.up_limit_kib = 64;

        let entries = registry.entries(&[torrent]);
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.info_hash, info_hash);
        assert!(entry.paused, "пауза должна попасть в сессию");
        assert_eq!(entry.selected_files, vec![0, 2]);
        assert_eq!(
            entry.priorities,
            vec![
                SessionFilePriority {
                    index: 1,
                    priority: FilePriority::Skip,
                },
                SessionFilePriority {
                    index: 2,
                    priority: FilePriority::High,
                },
            ]
        );
        assert_eq!(entry.down_limit_kib, 512);
        assert_eq!(entry.up_limit_kib, 64);
        assert!(matches!(entry.source, SessionSource::File { .. }));
    }

    #[test]
    fn entries_keep_all_selection_implicitly() {
        let dir = test_dir("all-selected");
        let registry = SessionRegistry::new(&dir);
        let info_hash = hash('7');
        registry
            .register_magnet(1, "magnet:?xt=urn:btih:bb")
            .expect("источник должен запоминаться");

        let entries = registry.entries(&[snapshot(1, &info_hash, TorrentState::Downloading)]);
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].selected_files.is_empty(),
            "полный выбор не пишется в сессию"
        );
        assert!(entries[0].priorities.is_empty());
    }

    #[test]
    fn torrent_without_source_is_skipped() {
        let dir = test_dir("no-source");
        let registry = SessionRegistry::new(&dir);
        let entries = registry.entries(&[snapshot(1, &hash('8'), TorrentState::Downloading)]);
        assert!(
            entries.is_empty(),
            "без источника и без копии .torrent торрент не восстановить"
        );
    }

    #[test]
    fn entries_are_sorted_by_info_hash() {
        let dir = test_dir("sorted");
        let registry = SessionRegistry::new(&dir);
        let first = hash('b');
        let second = hash('a');
        registry
            .register_magnet(1, "magnet:?xt=urn:btih:11")
            .expect("источник");
        registry
            .register_magnet(2, "magnet:?xt=urn:btih:22")
            .expect("источник");

        let entries = registry.entries(&[
            snapshot(1, &first, TorrentState::Downloading),
            snapshot(2, &second, TorrentState::Downloading),
        ]);
        assert_eq!(entries[0].info_hash, second);
        assert_eq!(entries[1].info_hash, first);
    }

    #[test]
    fn payload_hash_tracks_changes() {
        let first = vec![entry(&hash('9'))];
        let same = vec![entry(&hash('9'))];
        let mut changed = entry(&hash('9'));
        changed.paused = true;

        assert_eq!(payload_hash(&first), payload_hash(&same));
        assert_ne!(payload_hash(&first), payload_hash(&[changed]));
        assert_ne!(payload_hash(&first), payload_hash(&[]));
    }

    #[test]
    fn forget_and_len_follow_registrations() {
        let dir = test_dir("forget");
        let registry = SessionRegistry::new(&dir);
        assert!(registry.is_empty());

        registry
            .register_magnet(7, "magnet:?xt=urn:btih:cc")
            .expect("источник");
        assert_eq!(registry.len(), 1);
        assert!(matches!(
            registry.origin(7),
            Some(SessionSource::Magnet { .. })
        ));

        let removed = registry.forget(7);
        assert!(matches!(removed, Some(SessionSource::Magnet { .. })));
        assert!(registry.is_empty());
        assert!(registry.origin(7).is_none());
    }

    #[test]
    fn register_magnet_rejects_empty_link() {
        let dir = test_dir("empty-magnet");
        let registry = SessionRegistry::new(&dir);
        assert!(registry.register_magnet(1, "   ").is_err());
        assert!(registry.is_empty());
    }

    #[test]
    fn register_file_rejects_broken_hash() {
        let dir = test_dir("bad-hash");
        let registry = SessionRegistry::new(&dir);
        assert!(registry.register_file(1, "../../evil", b"data").is_err());
        assert!(registry.is_empty());
    }

    #[test]
    fn restore_save_path_requires_absolute_path() {
        let mut item = entry(&hash('a'));
        item.save_path = "  C:\\Загрузки  ".to_string();
        assert_eq!(
            restore_save_path(&item),
            Some(PathBuf::from("C:\\Загрузки"))
        );

        item.save_path = "относительный".to_string();
        assert_eq!(restore_save_path(&item), None);

        item.save_path = "   ".to_string();
        assert_eq!(restore_save_path(&item), None);
    }

    #[test]
    fn restore_files_deduplicates_and_sorts() {
        let mut item = entry(&hash('a'));
        assert_eq!(restore_files(&item), None);

        item.selected_files = vec![2, 0, 2];
        assert_eq!(restore_files(&item), Some(vec![0, 2]));
    }

    #[test]
    fn session_dir_is_inside_app_dir() {
        let dir = test_dir("paths");
        let store = SessionStore::new(&dir);
        assert_eq!(store.dir(), dir.join(SESSION_DIR_NAME));
        assert_eq!(
            store.file_path(),
            dir.join(SESSION_DIR_NAME).join(SESSION_FILE_NAME)
        );
        assert_eq!(
            store.data_dir(),
            dir.join(SESSION_DIR_NAME).join(SESSION_DATA_DIR_NAME)
        );
    }
}
