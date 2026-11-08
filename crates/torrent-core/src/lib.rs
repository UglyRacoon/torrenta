//! # torrent-core
//!
//! Самодостаточный движок торрент-клиента: разбор `.torrent`/magnet, HTTP и UDP
//! трекеры, peer wire protocol, хранилище кусков и планировщик загрузки.
//!
//! Крейт не зависит от Tauri и тестируется отдельно (`cargo test -p torrent-core`).
//! Слои:
//!
//! * [`bencode`] — кодирование/декодирование bencode (BEP 3);
//! * [`metainfo`] — разбор `.torrent`, подсчёт info-hash, раскладка файлов;
//! * [`magnet`] — разбор magnet-ссылок (BEP 9 magnet, BEP 53);
//! * [`tracker`] — announce по HTTP(S) и UDP (BEP 3, BEP 23, BEP 15);
//! * [`wire`] — сообщения peer wire protocol (BEP 3, BEP 10);
//! * [`bitfield`] — битовая карта кусков;
//! * [`picker`] — выбор кусков (rarest-first + приоритеты);
//! * [`storage`] — запись/чтение кусков в файлы на диске;
//! * [`engine`] — менеджер торрентов, сессии с пирами, статистика.

pub mod bencode;
pub mod bitfield;
pub mod engine;
pub mod magnet;
pub mod metainfo;
pub mod peerid;
pub mod picker;
pub(crate) mod selection;
pub mod storage;
pub mod tracker;
pub mod wire;

pub use bencode::{decode as bencode_decode, encode as bencode_encode, BencodeError, Value};
pub use bitfield::Bitfield;
pub use engine::{
    AddOptions, AddSource, EngineConfig, EngineError, EngineEvent, FilePriority, FileView,
    GlobalTotals, PreviewFile, TorrentManager, TorrentPreview, TorrentSnapshot, TorrentState,
};
pub use magnet::{Magnet, MagnetError};
pub use metainfo::{FileEntry, InfoHash, Metainfo, MetainfoError};
pub use picker::PiecePicker;
pub use storage::Storage;
pub use tracker::{AnnounceEvent, AnnounceRequest, AnnounceResponse, PeerAddr, TrackerError};
pub use wire::{Handshake, Message, WireError};
