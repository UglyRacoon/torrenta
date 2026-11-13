//! Распределённая хеш-таблица DHT (BEP 5).
//!
//! Узел общается по KRPC — bencode-словарям поверх UDP — и поддерживает запросы
//! `ping`, `find_node`, `get_peers`, `announce_peer`. Таблица маршрутизации
//! разбита на K-бакеты (K = 8) по общему префиксу идентификатора узла: бакет
//! содержит узлы, у которых с локальным идентификатором совпадает столько-то
//! старших бит. Мёртвые узлы (не ответившие на пинг) вытесняются.
//!
//! Найденные пиры складываются во внутренний кэш; вызывающий код забирает их
//! через [`Dht::take_peers`]. Фоновые задачи tokio принимают входящие запросы,
//! отвечают на них и периодически обновляют таблицу (bootstrap + пинг старых
//! узлов), поэтому [`Dht`] удобно хранить в `Arc`.
//!
//! Жизненный цикл: [`Dht::new`] → [`Dht::start`] → [`Dht::announce`] /
//! [`Dht::take_peers`] → [`Dht::shutdown`]. После остановки [`Dht::announce`]
//! возвращает [`DhtError::Closed`].
//!
//! Честные ограничения реализации:
//!
//! * поиск пиров — ограниченный итеративный обход: до `ANNOUNCE_ROUNDS` раундов
//!   `get_peers`, каждый следующий идёт по узлам, которых ещё не спрашивали.
//!   Полного обхода до схождения (как в Kademlia) нет: раундов ровно столько,
//!   чтобы уложиться в `ANNOUNCE_TIMEOUT`;
//! * `announce_peer` отправляется без ожидания ответа (best-effort);
//! * в компактном поле `nodes` передаются только IPv4-узлы (формат BEP 5),
//!   IPv6-пиры в `values` поддерживаются при разборе;
//! * нет BEP 42 (безопасные node id) и защиты от Sybil-атак.
//!
//! Сеть может быть недоступна: при неудачном bootstrap таблица остаётся пустой,
//! `announce` завершается ошибкой [`DhtError::Timeout`], а паники не происходит.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sha1::{Digest, Sha1};
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::bencode::{self, Value};
use crate::metainfo::InfoHash;

/// Публичные bootstrap-узлы, если пользователь не задал свои.
pub const DEFAULT_BOOTSTRAP: &[&str] = &[
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "router.utorrent.com:6881",
];

/// Длина идентификатора узла/инфо-хеша (SHA-1).
const ID_LEN: usize = 20;
/// Количество бит в идентификаторе — размер таблицы бакетов.
const ID_BITS: usize = ID_LEN * 8;
/// Размер одного K-бакета.
const BUCKET_SIZE: usize = 8;
/// Ограничение таблицы маршрутизации по умолчанию.
const DEFAULT_MAX_NODES: usize = 1024;
/// Сколько ошибок подряд считается смертью узла.
const MAX_NODE_FAILURES: u8 = 2;
/// Таймаут одного запроса (пинг, find_node, get_peers).
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
/// Общий таймаут `announce`.
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(5);
/// Сколько `announce` ждёт наполнения таблицы маршрутизации.
const BOOTSTRAP_WAIT: Duration = Duration::from_secs(3);
/// Период фонового обслуживания таблицы.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(15);
/// Возраст узла, после которого ему отправляется проверочный пинг.
const REFRESH_AFTER: Duration = Duration::from_secs(300);
/// Возраст узла, после которого он считается устаревшим и вытесняется.
const STALE_NODE: Duration = Duration::from_secs(900);
/// Максимальный возраст записи в кэше пиров.
const PEER_MAX_AGE: Duration = Duration::from_secs(600);
/// Сколько пиров храним на один info-hash.
const PEERS_PER_HASH_LIMIT: usize = 200;
/// Сколько info-hash держим в кэше одновременно.
const MAX_CACHED_HASHES: usize = 1024;
/// Количество узлов, которым отправляется `get_peers` за один раунд announce.
const ANNOUNCE_TARGETS: usize = 8;
/// Сколько раундов `get_peers` делает один `announce`.
///
/// Один раунд видит только [`ANNOUNCE_TARGETS`] ближайших узлов, а ответы
/// приносят новых кандидатов — несколько раундов заметно расширяют охват сети.
/// Раньше здесь был ровно один раунд, и на «редких» торрентах пиров находилось
/// меньше, чем в qBittorrent.
const ANNOUNCE_ROUNDS: usize = 3;
/// Сколько пиров отдаём в одном ответе `get_peers`.
const MAX_VALUES: usize = 50;
/// Сколько bootstrap-адресов рассматриваем за один проход.
const MAX_BOOTSTRAP_ADDRS: usize = 16;
/// Ниже этого количества узлов таблица считается недозаполненной.
const BOOTSTRAP_MIN_NODES: usize = 8;
/// Сколько узлов пингуем за один проход обслуживания.
const MAX_REFRESH_PINGS: usize = 4;
/// Длина токена для `announce_peer`.
const TOKEN_LEN: usize = 8;
/// Длина секрета, из которого считается токен.
const TOKEN_SECRET_LEN: usize = 16;
/// Размер буфера приёма UDP: один datagram целиком.
const RECV_BUFFER: usize = 65_536;
/// Размер компактного узла: 20 байт id + 4 байта IPv4 + 2 байта порта.
const COMPACT_NODE_LEN: usize = 26;
/// Размер компактного IPv4-пира: 4 байта адреса + 2 байта порта.
const COMPACT_PEER_V4_LEN: usize = 6;
/// Размер компактного IPv6-пира: 16 байт адреса + 2 байта порта.
const COMPACT_PEER_V6_LEN: usize = 18;

/// Один KRPC-идентификатор узла.
type NodeId = [u8; ID_LEN];

/// Настройки узла DHT.
#[derive(Debug, Clone)]
pub struct DhtConfig {
    /// UDP-порт; `0` — выбрать свободный автоматически.
    pub port: u16,
    /// Bootstrap-адреса вида `host:port`. Пусто — [`DEFAULT_BOOTSTRAP`].
    pub bootstrap: Vec<String>,
    /// Верхняя граница количества узлов в таблице маршрутизации.
    pub max_nodes: usize,
}

impl Default for DhtConfig {
    fn default() -> Self {
        Self {
            port: 0,
            bootstrap: Vec::new(),
            max_nodes: DEFAULT_MAX_NODES,
        }
    }
}

/// Снимок состояния узла.
#[derive(Debug, Clone, Default)]
pub struct DhtStats {
    /// Запущен ли узел (`start` вызван, `shutdown` — нет).
    pub running: bool,
    /// Количество узлов в таблице маршрутизации.
    pub nodes: usize,
    /// Суммарное количество закэшированных адресов пиров.
    pub peer_cache: usize,
    /// Сколько запросов мы отправили.
    pub queries: u64,
    /// Сколько ответов на наши запросы получили.
    pub responses: u64,
}

/// Ошибки DHT.
#[derive(Debug, Error)]
pub enum DhtError {
    /// Узел не запущен или уже остановлен.
    #[error("DHT не запущен или остановлен")]
    Closed,
    /// Ошибка UDP-сокета.
    #[error("сетевая ошибка DHT: {0}")]
    Io(#[from] std::io::Error),
    /// Ни один узел не ответил за отведённое время.
    #[error("истёк таймаут ожидания ответа DHT")]
    Timeout,
    /// Некорректное KRPC-сообщение или параметры вызова.
    #[error("некорректный KRPC: {0}")]
    Protocol(String),
    /// Удалённый узел вернул KRPC-ошибку.
    #[error("узел DHT вернул ошибку: {0}")]
    Remote(String),
}

/// Узел распределённой хеш-таблицы.
///
/// Конструктор создаёт только состояние; сеть поднимается в [`Dht::start`].
pub struct Dht {
    config: DhtConfig,
    node_id: NodeId,
    bind_ip: IpAddr,
    token_secret: [u8; TOKEN_SECRET_LEN],
    start_lock: tokio::sync::Mutex<()>,
    socket: Mutex<Option<Arc<UdpSocket>>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    stop: Notify,
    running: AtomicBool,
    routing: Mutex<RoutingTable>,
    peers: Mutex<PeerCache>,
    pending: Mutex<HashMap<u16, PendingEntry>>,
    transactions: AtomicU16,
    queries: AtomicU64,
    responses: AtomicU64,
}

impl Dht {
    /// Создаёт узел со случайным идентификатором. Сеть не трогается.
    pub fn new(config: DhtConfig) -> Self {
        let node_id = rand::random::<NodeId>();
        let token_secret = rand::random::<[u8; TOKEN_SECRET_LEN]>();
        let max_nodes = config.max_nodes.max(1);
        Self {
            config,
            node_id,
            bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            token_secret,
            start_lock: tokio::sync::Mutex::new(()),
            socket: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
            stop: Notify::new(),
            running: AtomicBool::new(false),
            routing: Mutex::new(RoutingTable::new(node_id, max_nodes)),
            peers: Mutex::new(PeerCache::default()),
            pending: Mutex::new(HashMap::new()),
            transactions: AtomicU16::new(rand::random::<u16>()),
            queries: AtomicU64::new(0),
            responses: AtomicU64::new(0),
        }
    }

    /// Идентификатор этого узла.
    pub fn node_id(&self) -> [u8; 20] {
        self.node_id
    }

    /// Привязывает UDP-сокет и запускает фоновые задачи.
    ///
    /// Возвращает фактический порт. Повторный вызов возвращает тот же порт.
    pub async fn start(self: &Arc<Self>) -> std::io::Result<u16> {
        let _guard = self.start_lock.lock().await;
        if let Some(port) = self.bound_port() {
            return Ok(port);
        }
        let bind_address = SocketAddr::new(self.bind_ip, self.config.port);
        let socket = Arc::new(UdpSocket::bind(bind_address).await?);
        let port = socket.local_addr()?.port();
        *lock(&self.socket) = Some(socket.clone());
        self.running.store(true, Ordering::SeqCst);
        let receiver = tokio::spawn(run_receiver(self.clone(), socket));
        let maintenance = tokio::spawn(run_maintenance(self.clone()));
        *lock(&self.tasks) = vec![receiver, maintenance];
        Ok(port)
    }

    /// Ищет пиров по info-hash и анонсирует свой порт ближайшим узлам.
    ///
    /// Поиск итеративный: каждый раунд — `get_peers` узлам, которых ещё не
    /// спрашивали, начиная с ближайших к цели. Ответы приносят новых
    /// кандидатов (они попадают в таблицу маршрутизации при обработке
    /// ответа), поэтому следующие раунды уходят глубже к цели. Раньше здесь был
    /// один раунд, и на торрентах с редкими пирами этого не хватало.
    ///
    /// Найденные адреса складываются во внутренний кэш; забрать их можно
    /// через [`Dht::take_peers`]. Ошибка [`DhtError::Timeout`] означает, что
    /// ни один узел не ответил (например, таблица маршрутизации пуста).
    pub async fn announce(&self, info_hash: InfoHash, port: u16) -> Result<(), DhtError> {
        if !self.is_running() {
            return Err(DhtError::Closed);
        }
        if port == 0 {
            return Err(DhtError::Protocol(
                "порт пира не может быть нулевым".to_string(),
            ));
        }
        let socket = self.current_socket()?;
        if self.known_nodes(info_hash).await.is_empty() {
            return Err(DhtError::Timeout);
        }

        let (sender, mut receiver) = mpsc::unbounded_channel();
        let deadline = Instant::now() + ANNOUNCE_TIMEOUT;
        // Кому уже отправили `get_peers`: второй раз не спрашиваем, иначе
        // раунды будут топтаться на одних и тех же узлах.
        let mut visited: HashSet<SocketAddr> = HashSet::new();
        let mut tokens: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
        let mut answered = 0usize;
        let mut last_error = None;

        for _ in 0..ANNOUNCE_ROUNDS {
            // Бюджет времени на весь announce общий: новый раунд не начинаем,
            // если он уже исчерпан.
            if deadline.checked_duration_since(Instant::now()).is_none() {
                break;
            }
            let candidates: Vec<SocketAddr> = lock(&self.routing)
                .closest(info_hash.as_bytes(), ANNOUNCE_TARGETS)
                .into_iter()
                .map(|node| node.addr)
                .filter(|addr| !visited.contains(addr))
                .collect();
            if candidates.is_empty() {
                // Спрашивать больше некого — итеративный обход сошёлся.
                break;
            }

            let mut pending = Vec::with_capacity(candidates.len());
            for addr in candidates {
                visited.insert(addr);
                let transaction = self.register_pending(addr, &sender);
                let packet = encode_query(
                    transaction,
                    &self.node_id,
                    &QueryKind::GetPeers {
                        info_hash: *info_hash.as_bytes(),
                    },
                );
                match socket.send_to(&packet, addr).await {
                    Ok(_) => {
                        self.queries.fetch_add(1, Ordering::Relaxed);
                        pending.push(transaction);
                    }
                    Err(error) => {
                        lock(&self.pending).remove(&transaction);
                        last_error = Some(DhtError::Io(error));
                    }
                }
            }
            if pending.is_empty() {
                break;
            }

            let expected = pending.len();
            let mut round_answers = 0usize;
            while round_answers < expected {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                match timeout(remaining, receiver.recv()).await {
                    Ok(Some((PendingReply::Response(response), from))) => {
                        round_answers += 1;
                        answered += 1;
                        if let Some(token) = response.token {
                            if !token.is_empty() {
                                tokens.push((from, token));
                            }
                        }
                        self.store_peers(info_hash, &response.values);
                    }
                    Ok(Some((PendingReply::Error(_), _from))) => {
                        round_answers += 1;
                        answered += 1;
                    }
                    // Канал закрылся: узел остановили.
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            for transaction in pending {
                lock(&self.pending).remove(&transaction);
            }

            // В этом раунде никто не ответил: таблица не расширится, идти
            // дальше некуда.
            if round_answers == 0 {
                break;
            }
            // Пиров уже достаточно: лишние раунды только тратят время.
            if self.cached_peers(info_hash.as_bytes()).len() >= MAX_VALUES {
                break;
            }
        }

        if answered == 0 {
            return Err(last_error.unwrap_or(DhtError::Timeout));
        }

        for (addr, token) in tokens {
            self.send_announce_peer(&socket, addr, info_hash, port, token)
                .await;
        }
        Ok(())
    }

    /// Забирает найденные адреса пиров и очищает кэш для этого info-hash.
    pub fn take_peers(&self, info_hash: &InfoHash) -> Vec<SocketAddr> {
        lock(&self.peers).take(info_hash)
    }

    /// Текущая статистика узла.
    pub fn stats(&self) -> DhtStats {
        DhtStats {
            running: self.is_running(),
            nodes: lock(&self.routing).len(),
            peer_cache: lock(&self.peers).len(),
            queries: self.queries.load(Ordering::Relaxed),
            responses: self.responses.load(Ordering::Relaxed),
        }
    }

    /// Останавливает фоновые задачи и освобождает сокет. Идемпотентно.
    pub async fn shutdown(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.stop.notify_waiters();
        let tasks = std::mem::take(&mut *lock(&self.tasks));
        for task in tasks {
            task.abort();
        }
        *lock(&self.socket) = None;
        lock(&self.pending).clear();
        // Даём отменённым задачам шанс завершиться до возврата из shutdown.
        tokio::task::yield_now().await;
    }

    /// Тестовый вариант: слушать только loopback, чтобы не трогать сеть хоста.
    #[cfg(test)]
    fn bind_loopback(mut self) -> Self {
        self.bind_ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        self
    }

    /// Порт, если узел уже запущен.
    fn bound_port(&self) -> Option<u16> {
        if !self.running.load(Ordering::SeqCst) {
            return None;
        }
        let socket = lock(&self.socket);
        let address = socket.as_ref()?.local_addr().ok()?;
        Some(address.port())
    }

    /// Признак работы узла.
    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Текущий сокет или [`DhtError::Closed`].
    fn current_socket(&self) -> Result<Arc<UdpSocket>, DhtError> {
        if !self.is_running() {
            return Err(DhtError::Closed);
        }
        lock(&self.socket).clone().ok_or(DhtError::Closed)
    }

    /// Ждёт, пока таблица маршрутизации наполнится (bootstrap в фоне).
    async fn known_nodes(&self, info_hash: InfoHash) -> Vec<Node> {
        let deadline = Instant::now() + BOOTSTRAP_WAIT;
        loop {
            let nodes = lock(&self.routing).closest(info_hash.as_bytes(), ANNOUNCE_TARGETS);
            if !nodes.is_empty() {
                return nodes;
            }
            if !self.is_running() || Instant::now() >= deadline {
                return Vec::new();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Отправляет один запрос и ждёт ответ (используется bootstrap и пингами).
    async fn query_round(
        &self,
        addr: SocketAddr,
        kind: QueryKind,
    ) -> Result<KrpcResponse, DhtError> {
        let socket = self.current_socket()?;
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let transaction = self.register_pending(addr, &sender);
        let packet = encode_query(transaction, &self.node_id, &kind);
        let outcome = match socket.send_to(&packet, addr).await {
            Err(error) => Err(DhtError::Io(error)),
            Ok(_) => {
                self.queries.fetch_add(1, Ordering::Relaxed);
                match timeout(QUERY_TIMEOUT, receiver.recv()).await {
                    Ok(Some((PendingReply::Response(response), _from))) => Ok(response),
                    Ok(Some((PendingReply::Error(message), _from))) => {
                        Err(DhtError::Remote(message))
                    }
                    Ok(None) => Err(DhtError::Closed),
                    Err(_) => Err(DhtError::Timeout),
                }
            }
        };
        lock(&self.pending).remove(&transaction);
        outcome
    }

    /// Регистрирует ожидание ответа с данным адресом и возвращает id транзакции.
    fn register_pending(
        &self,
        addr: SocketAddr,
        reply: &mpsc::UnboundedSender<(PendingReply, SocketAddr)>,
    ) -> u16 {
        let transaction = self.allocate_transaction();
        lock(&self.pending).insert(
            transaction,
            PendingEntry {
                addr,
                reply: reply.clone(),
            },
        );
        transaction
    }

    /// Выдаёт свободный id транзакции.
    fn allocate_transaction(&self) -> u16 {
        for _ in 0..=u16::MAX {
            let transaction = self.transactions.fetch_add(1, Ordering::Relaxed);
            if !lock(&self.pending).contains_key(&transaction) {
                return transaction;
            }
        }
        // Практически недостижимо: 65 536 одновременно ожидаемых запросов.
        // Всё равно не паникуем — отдаём очередное значение.
        self.transactions.fetch_add(1, Ordering::Relaxed)
    }

    /// Отправляет `announce_peer` без ожидания ответа (best-effort).
    async fn send_announce_peer(
        &self,
        socket: &UdpSocket,
        addr: SocketAddr,
        info_hash: InfoHash,
        port: u16,
        token: Vec<u8>,
    ) {
        if !self.is_running() {
            return;
        }
        let transaction = self.allocate_transaction();
        let packet = encode_query(
            transaction,
            &self.node_id,
            &QueryKind::AnnouncePeer {
                info_hash: *info_hash.as_bytes(),
                port,
                token,
            },
        );
        if socket.send_to(&packet, addr).await.is_ok() {
            self.queries.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Складывает найденные адреса пиров в кэш.
    fn store_peers(&self, info_hash: InfoHash, addresses: &[SocketAddr]) {
        if addresses.is_empty() {
            return;
        }
        let mut peers = lock(&self.peers);
        for address in addresses {
            peers.insert(info_hash, *address);
        }
    }

    /// Обрабатывает входящий UDP-datagram.
    async fn handle_datagram(&self, socket: &UdpSocket, data: &[u8], from: SocketAddr) {
        let message = match decode_message(data) {
            Ok(message) => message,
            // Мусор и чужие форматы молча отбрасываем.
            Err(_) => return,
        };
        match message {
            KrpcMessage::Query { transaction, query } => {
                lock(&self.routing).insert(from, query.id);
                let packet = match self.answer_query(&query, from) {
                    Answer::Reply(body) => encode_response(transaction, &self.node_id, &body),
                    Answer::Error { code, message } => encode_error(transaction, code, &message),
                };
                let _ = socket.send_to(&packet, from).await;
            }
            KrpcMessage::Response {
                transaction,
                response,
            } => self.handle_response(transaction, from, response),
            KrpcMessage::Error {
                transaction,
                message,
            } => self.handle_remote_error(transaction, from, message),
        }
    }

    /// Готовит ответ на входящий запрос.
    fn answer_query(&self, query: &KrpcQuery, from: SocketAddr) -> Answer {
        match &query.kind {
            QueryKind::Ping => Answer::Reply(ResponseBody::Ping),
            QueryKind::FindNode { target } => Answer::Reply(ResponseBody::FindNode(
                self.closest_nodes(target, BUCKET_SIZE),
            )),
            QueryKind::GetPeers { info_hash } => {
                let values = self.cached_peers(info_hash);
                let token = make_token(&self.token_secret, from.ip());
                let nodes = if values.is_empty() {
                    self.closest_nodes(info_hash, BUCKET_SIZE)
                } else {
                    Vec::new()
                };
                Answer::Reply(ResponseBody::GetPeers {
                    token,
                    values,
                    nodes,
                })
            }
            QueryKind::AnnouncePeer {
                info_hash,
                port,
                token,
            } => {
                let expected = make_token(&self.token_secret, from.ip());
                if token.as_slice() != expected.as_slice() {
                    return Answer::Error {
                        code: 203,
                        message: "неверный токен announce_peer".to_string(),
                    };
                }
                let announced = if *port == 0 { from.port() } else { *port };
                lock(&self.peers).insert(
                    InfoHash::new(*info_hash),
                    SocketAddr::new(from.ip(), announced),
                );
                Answer::Reply(ResponseBody::AnnouncePeer)
            }
        }
    }

    /// Ближайшие к цели узлы таблицы в компактном представлении.
    fn closest_nodes(&self, target: &NodeId, limit: usize) -> Vec<(SocketAddr, NodeId)> {
        lock(&self.routing)
            .closest(target, limit)
            .into_iter()
            .map(|node| (node.addr, node.id))
            .collect()
    }

    /// Адреса пиров из кэша для ответа на `get_peers`.
    fn cached_peers(&self, info_hash: &NodeId) -> Vec<SocketAddr> {
        let hash = InfoHash::new(*info_hash);
        lock(&self.peers).peek(&hash, MAX_VALUES)
    }

    /// Учитывает ответ: обновляет таблицу и будит ожидающего.
    fn handle_response(&self, transaction: u16, from: SocketAddr, response: KrpcResponse) {
        let entry = lock(&self.pending).remove(&transaction);
        let waiting = match entry {
            Some(entry) if entry.addr == from => Some(entry),
            // Ответ от узла, которому мы не отправляли запрос, игнорируем.
            Some(_) => return,
            None => None,
        };
        lock(&self.routing).insert(from, response.id);
        for (addr, id) in &response.nodes {
            lock(&self.routing).insert(*addr, *id);
        }
        if let Some(entry) = waiting {
            self.responses.fetch_add(1, Ordering::Relaxed);
            let _ = entry.reply.send((PendingReply::Response(response), from));
        }
    }

    /// Учитывает KRPC-ошибку удалённого узла.
    fn handle_remote_error(&self, transaction: u16, from: SocketAddr, message: String) {
        if let Some(entry) = lock(&self.pending).remove(&transaction) {
            if entry.addr == from {
                let _ = entry.reply.send((PendingReply::Error(message), from));
            }
        }
    }

    /// Один проход обслуживания: bootstrap и проверка старых узлов.
    async fn maintenance_tick(&self) {
        if !self.is_running() {
            return;
        }
        let node_count = lock(&self.routing).len();
        if node_count < BOOTSTRAP_MIN_NODES {
            self.bootstrap_once().await;
            if !self.is_running() {
                return;
            }
        }
        let stale = lock(&self.routing).stale(MAX_REFRESH_PINGS);
        for node in stale {
            if !self.is_running() {
                return;
            }
            if self.query_round(node.addr, QueryKind::Ping).await.is_err() {
                lock(&self.routing).mark_failure(node.addr);
            }
        }
        lock(&self.routing).remove_dead();
        lock(&self.peers).prune(PEER_MAX_AGE);
    }

    /// Опрашивает bootstrap-узлы (свои или стандартные).
    async fn bootstrap_once(&self) {
        if self.config.bootstrap.is_empty() {
            for spec in DEFAULT_BOOTSTRAP.iter().take(MAX_BOOTSTRAP_ADDRS) {
                if !self.bootstrap_spec(spec).await {
                    return;
                }
            }
        } else {
            for spec in self.config.bootstrap.iter().take(MAX_BOOTSTRAP_ADDRS) {
                if !self.bootstrap_spec(spec).await {
                    return;
                }
            }
        }
    }

    /// Разрешает один bootstrap-адрес и опрашивает его.
    ///
    /// Возвращает `false`, если узел остановлен и работу нужно прекратить.
    async fn bootstrap_spec(&self, spec: &str) -> bool {
        let Ok(addresses) = tokio::net::lookup_host(spec).await else {
            return true;
        };
        for addr in addresses.take(MAX_BOOTSTRAP_ADDRS) {
            if !self.is_running() {
                return false;
            }
            if !is_routable(addr) || self.contains_node(addr) {
                continue;
            }
            let _ = self
                .query_round(
                    addr,
                    QueryKind::FindNode {
                        target: self.node_id,
                    },
                )
                .await;
            if lock(&self.routing).len() >= BOOTSTRAP_MIN_NODES {
                return true;
            }
        }
        true
    }

    /// Есть ли уже такой адрес в таблице.
    fn contains_node(&self, addr: SocketAddr) -> bool {
        lock(&self.routing).contains_addr(addr)
    }
}

/// Приём входящих datagram и фоновая поддержка таблицы.
async fn run_receiver(dht: Arc<Dht>, socket: Arc<UdpSocket>) {
    let mut buffer = vec![0u8; RECV_BUFFER];
    loop {
        let received = tokio::select! {
            () = dht.stop.notified() => break,
            result = socket.recv_from(&mut buffer) => result,
        };
        match received {
            Ok((length, from)) => {
                let datagram = &buffer[..length];
                dht.handle_datagram(&socket, datagram, from).await;
            }
            // На Windows ICMP «порт недоступен» может дать ECONNRESET на
            // следующем recv_from — это не повод останавливать узел.
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}

/// Периодическое обслуживание таблицы маршрутизации и кэша пиров.
async fn run_maintenance(dht: Arc<Dht>) {
    let mut ticker = tokio::time::interval(MAINTENANCE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = dht.stop.notified() => break,
            _ = ticker.tick() => {
                if !dht.is_running() {
                    break;
                }
                dht.maintenance_tick().await;
            }
        }
    }
}

/// Ожидание ответа на конкретный запрос.
#[derive(Debug)]
enum PendingReply {
    Response(KrpcResponse),
    Error(String),
}

/// Ожидающий запрос: адрес получателя и канал доставки ответа.
struct PendingEntry {
    addr: SocketAddr,
    reply: mpsc::UnboundedSender<(PendingReply, SocketAddr)>,
}

/// Узел таблицы маршрутизации.
#[derive(Debug, Clone)]
struct Node {
    id: NodeId,
    addr: SocketAddr,
    last_seen: Instant,
    failures: u8,
}

impl Node {
    /// Узел считается мёртвым после [`MAX_NODE_FAILURES`] ошибок или
    /// длительного молчания.
    fn is_dead(&self) -> bool {
        self.failures >= MAX_NODE_FAILURES || self.last_seen.elapsed() > STALE_NODE
    }

    /// Пора ли проверить узел пингом.
    fn is_stale(&self, after: Duration) -> bool {
        self.last_seen.elapsed() > after
    }
}

/// Таблица маршрутизации: K-бакеты по общему префиксу с локальным id.
struct RoutingTable {
    local_id: NodeId,
    max_nodes: usize,
    buckets: Vec<Vec<Node>>,
}

impl RoutingTable {
    fn new(local_id: NodeId, max_nodes: usize) -> Self {
        Self {
            local_id,
            max_nodes: max_nodes.max(1),
            buckets: vec![Vec::new(); ID_BITS + 1],
        }
    }

    /// Общее количество узлов.
    fn len(&self) -> usize {
        self.buckets.iter().map(Vec::len).sum()
    }

    /// Индекс бакета: количество старших совпадающих бит идентификатора.
    fn bucket_index(&self, id: &NodeId) -> usize {
        let mut bits = 0usize;
        for (local, remote) in self.local_id.iter().zip(id.iter()) {
            let diff = local ^ remote;
            if diff != 0 {
                return bits + diff.leading_zeros() as usize;
            }
            bits += 8;
        }
        bits
    }

    /// Добавляет узел. Возвращает `false`, если места нет или адрес негоден.
    fn insert(&mut self, addr: SocketAddr, id: NodeId) -> bool {
        if id == self.local_id || !is_routable(addr) {
            return false;
        }
        let total = self.len();
        let index = self.bucket_index(&id);
        let bucket = &mut self.buckets[index];
        if let Some(existing) = bucket
            .iter_mut()
            .find(|node| node.id == id || node.addr == addr)
        {
            existing.addr = addr;
            existing.last_seen = Instant::now();
            existing.failures = 0;
            return true;
        }
        if bucket.len() < BUCKET_SIZE && total < self.max_nodes {
            bucket.push(Node {
                id,
                addr,
                last_seen: Instant::now(),
                failures: 0,
            });
            return true;
        }
        // Бакет полон: вытесняем мёртвый узел, если он есть.
        if let Some(slot) = bucket.iter_mut().find(|node| node.is_dead()) {
            *slot = Node {
                id,
                addr,
                last_seen: Instant::now(),
                failures: 0,
            };
            return true;
        }
        false
    }

    /// Есть ли узел с таким адресом.
    fn contains_addr(&self, addr: SocketAddr) -> bool {
        self.buckets.iter().flatten().any(|node| node.addr == addr)
    }

    /// Ближайшие к цели узлы (по XOR-расстоянию).
    fn closest(&self, target: &NodeId, count: usize) -> Vec<Node> {
        let mut nodes: Vec<Node> = self.buckets.iter().flatten().cloned().collect();
        nodes.sort_by_key(|node| xor_distance(&node.id, target));
        nodes.truncate(count);
        nodes
    }

    /// Увеличивает счётчик ошибок узла с данным адресом.
    fn mark_failure(&mut self, addr: SocketAddr) {
        for bucket in &mut self.buckets {
            if let Some(node) = bucket.iter_mut().find(|node| node.addr == addr) {
                node.failures = node.failures.saturating_add(1);
                return;
            }
        }
    }

    /// Удаляет мёртвые узлы и возвращает количество удалённых.
    fn remove_dead(&mut self) -> usize {
        let mut removed = 0;
        for bucket in &mut self.buckets {
            let before = bucket.len();
            bucket.retain(|node| !node.is_dead());
            removed += before - bucket.len();
        }
        removed
    }

    /// Узлы, которые давно не подавали признаков жизни.
    fn stale(&self, limit: usize) -> Vec<Node> {
        let mut nodes: Vec<Node> = self
            .buckets
            .iter()
            .flatten()
            .filter(|node| node.is_stale(REFRESH_AFTER))
            .cloned()
            .collect();
        nodes.sort_by_key(|node| node.last_seen);
        nodes.truncate(limit);
        nodes
    }
}

/// Запись кэша пиров.
#[derive(Debug, Clone)]
struct CachedPeer {
    addr: SocketAddr,
    seen: Instant,
}

/// Кэш адресов пиров по info-hash.
#[derive(Default)]
struct PeerCache {
    entries: BTreeMap<InfoHash, Vec<CachedPeer>>,
}

impl PeerCache {
    fn insert(&mut self, info_hash: InfoHash, addr: SocketAddr) {
        if !is_routable(addr) {
            return;
        }
        {
            let list = self.entries.entry(info_hash).or_default();
            if let Some(existing) = list.iter_mut().find(|peer| peer.addr == addr) {
                existing.seen = Instant::now();
            } else {
                if list.len() >= PEERS_PER_HASH_LIMIT {
                    if let Some((index, _)) =
                        list.iter().enumerate().min_by_key(|(_, peer)| peer.seen)
                    {
                        list.swap_remove(index);
                    }
                }
                list.push(CachedPeer {
                    addr,
                    seen: Instant::now(),
                });
            }
        }
        // Не даём кэшу расти бесконечно: выкидываем самые «младшие» хеши.
        while self.entries.len() > MAX_CACHED_HASHES {
            let Some(first) = self.entries.keys().next().copied() else {
                break;
            };
            self.entries.remove(&first);
        }
    }

    /// Адреса без изъятия из кэша.
    fn peek(&self, info_hash: &InfoHash, limit: usize) -> Vec<SocketAddr> {
        let Some(list) = self.entries.get(info_hash) else {
            return Vec::new();
        };
        list.iter().take(limit).map(|peer| peer.addr).collect()
    }

    /// Забирает адреса и удаляет запись.
    fn take(&mut self, info_hash: &InfoHash) -> Vec<SocketAddr> {
        let list = self.entries.remove(info_hash);
        match list {
            Some(list) => list.into_iter().map(|peer| peer.addr).collect(),
            None => Vec::new(),
        }
    }

    /// Суммарное количество адресов.
    fn len(&self) -> usize {
        self.entries.values().map(Vec::len).sum()
    }

    /// Удаляет записи старше `max_age`.
    fn prune(&mut self, max_age: Duration) {
        self.entries.retain(|_, list| {
            list.retain(|peer| peer.seen.elapsed() < max_age);
            !list.is_empty()
        });
    }
}

/// Вид KRPC-запроса.
#[derive(Debug, Clone)]
enum QueryKind {
    Ping,
    FindNode {
        target: NodeId,
    },
    GetPeers {
        info_hash: NodeId,
    },
    AnnouncePeer {
        info_hash: NodeId,
        port: u16,
        token: Vec<u8>,
    },
}

/// Разобранный входящий запрос.
#[derive(Debug, Clone)]
struct KrpcQuery {
    id: NodeId,
    kind: QueryKind,
}

/// Разобранный ответ.
#[derive(Debug, Clone)]
struct KrpcResponse {
    id: NodeId,
    nodes: Vec<(SocketAddr, NodeId)>,
    values: Vec<SocketAddr>,
    token: Option<Vec<u8>>,
}

/// Разобранное KRPC-сообщение.
#[derive(Debug, Clone)]
enum KrpcMessage {
    Query {
        transaction: u16,
        query: KrpcQuery,
    },
    Response {
        transaction: u16,
        response: KrpcResponse,
    },
    Error {
        transaction: u16,
        message: String,
    },
}

/// Тело ответа на запрос.
enum ResponseBody {
    Ping,
    FindNode(Vec<(SocketAddr, NodeId)>),
    GetPeers {
        token: Vec<u8>,
        values: Vec<SocketAddr>,
        nodes: Vec<(SocketAddr, NodeId)>,
    },
    AnnouncePeer,
}

/// Результат обработки запроса: ответ или KRPC-ошибка.
enum Answer {
    Reply(ResponseBody),
    Error { code: i64, message: String },
}

/// Блокировка мьютекса без паники при отравленном мьютексе.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Пригоден ли адрес для таблицы маршрутизации и кэша пиров.
fn is_routable(addr: SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(ip) => !(ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast()),
        IpAddr::V6(ip) => !(ip.is_unspecified() || ip.is_multicast()),
    }
}

/// XOR-расстояние между идентификаторами (метрика Kademlia).
fn xor_distance(left: &NodeId, right: &NodeId) -> NodeId {
    let mut distance = [0u8; ID_LEN];
    for (slot, (a, b)) in distance.iter_mut().zip(left.iter().zip(right.iter())) {
        *slot = a ^ b;
    }
    distance
}

/// Строит bencode-словарь из пар ключ-значение.
fn bencode_dict(pairs: Vec<(&str, Value)>) -> Value {
    let mut map: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
    for (key, value) in pairs {
        map.insert(key.as_bytes().to_vec(), value);
    }
    Value::Dict(map)
}

/// Кодирует KRPC-запрос.
fn encode_query(transaction: u16, own_id: &NodeId, query: &QueryKind) -> Vec<u8> {
    let mut arguments: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
    arguments.insert(b"id".to_vec(), Value::Bytes(own_id.to_vec()));
    let name = match query {
        QueryKind::Ping => "ping",
        QueryKind::FindNode { target } => {
            arguments.insert(b"target".to_vec(), Value::Bytes(target.to_vec()));
            "find_node"
        }
        QueryKind::GetPeers { info_hash } => {
            arguments.insert(b"info_hash".to_vec(), Value::Bytes(info_hash.to_vec()));
            "get_peers"
        }
        QueryKind::AnnouncePeer {
            info_hash,
            port,
            token,
        } => {
            arguments.insert(b"implied_port".to_vec(), Value::Int(0));
            arguments.insert(b"info_hash".to_vec(), Value::Bytes(info_hash.to_vec()));
            arguments.insert(b"port".to_vec(), Value::Int(i64::from(*port)));
            arguments.insert(b"token".to_vec(), Value::Bytes(token.clone()));
            "announce_peer"
        }
    };
    let message = bencode_dict(vec![
        ("a", Value::Dict(arguments)),
        ("q", Value::string(name)),
        ("t", Value::Bytes(transaction.to_be_bytes().to_vec())),
        ("y", Value::string("q")),
    ]);
    bencode::encode(&message)
}

/// Кодирует KRPC-ответ.
fn encode_response(transaction: u16, own_id: &NodeId, body: &ResponseBody) -> Vec<u8> {
    let mut response: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
    response.insert(b"id".to_vec(), Value::Bytes(own_id.to_vec()));
    match body {
        ResponseBody::Ping | ResponseBody::AnnouncePeer => {}
        ResponseBody::FindNode(nodes) => {
            response.insert(b"nodes".to_vec(), Value::Bytes(encode_compact_nodes(nodes)));
        }
        ResponseBody::GetPeers {
            token,
            values,
            nodes,
        } => {
            response.insert(b"token".to_vec(), Value::Bytes(token.clone()));
            if values.is_empty() {
                response.insert(b"nodes".to_vec(), Value::Bytes(encode_compact_nodes(nodes)));
            } else {
                response.insert(
                    b"values".to_vec(),
                    Value::List(values.iter().copied().map(encode_compact_value).collect()),
                );
            }
        }
    }
    bencode::encode(&bencode_dict(vec![
        ("r", Value::Dict(response)),
        ("t", Value::Bytes(transaction.to_be_bytes().to_vec())),
        ("y", Value::string("r")),
    ]))
}

/// Кодирует KRPC-ошибку.
fn encode_error(transaction: u16, code: i64, message: &str) -> Vec<u8> {
    bencode::encode(&bencode_dict(vec![
        (
            "e",
            Value::List(vec![Value::Int(code), Value::string(message)]),
        ),
        ("t", Value::Bytes(transaction.to_be_bytes().to_vec())),
        ("y", Value::string("e")),
    ]))
}

/// Кодирует компактный список узлов (только IPv4, формат BEP 5).
fn encode_compact_nodes(nodes: &[(SocketAddr, NodeId)]) -> Vec<u8> {
    let mut compact = Vec::with_capacity(nodes.len() * COMPACT_NODE_LEN);
    for (addr, id) in nodes {
        if let SocketAddr::V4(v4) = addr {
            compact.extend_from_slice(id);
            compact.extend_from_slice(&v4.ip().octets());
            compact.extend_from_slice(&v4.port().to_be_bytes());
        }
    }
    compact
}

/// Кодирует один компактный адрес пира для поля `values`.
fn encode_compact_value(addr: SocketAddr) -> Value {
    match addr {
        SocketAddr::V4(v4) => {
            let mut bytes = Vec::with_capacity(COMPACT_PEER_V4_LEN);
            bytes.extend_from_slice(&v4.ip().octets());
            bytes.extend_from_slice(&v4.port().to_be_bytes());
            Value::Bytes(bytes)
        }
        SocketAddr::V6(v6) => {
            let mut bytes = Vec::with_capacity(COMPACT_PEER_V6_LEN);
            bytes.extend_from_slice(&v6.ip().octets());
            bytes.extend_from_slice(&v6.port().to_be_bytes());
            Value::Bytes(bytes)
        }
    }
}

/// Разбирает компактный список узлов; неполный хвост отбрасывается.
fn decode_compact_nodes(data: &[u8]) -> Vec<(SocketAddr, NodeId)> {
    let mut nodes = Vec::new();
    for chunk in data.chunks_exact(COMPACT_NODE_LEN) {
        let mut id = [0u8; ID_LEN];
        id.copy_from_slice(&chunk[..ID_LEN]);
        let octets = [chunk[20], chunk[21], chunk[22], chunk[23]];
        let port = u16::from_be_bytes([chunk[24], chunk[25]]);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port);
        if id != [0u8; ID_LEN] && is_routable(addr) {
            nodes.push((addr, id));
        }
    }
    nodes
}

/// Разбирает компактный адрес пира (IPv4 — 6 байт, IPv6 — 18 байт).
fn decode_compact_peer(value: &Value) -> Option<SocketAddr> {
    let bytes = value.as_bytes()?;
    let addr = match bytes.len() {
        COMPACT_PEER_V4_LEN => {
            let octets = [bytes[0], bytes[1], bytes[2], bytes[3]];
            let port = u16::from_be_bytes([bytes[4], bytes[5]]);
            SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port)
        }
        COMPACT_PEER_V6_LEN => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&bytes[..16]);
            let port = u16::from_be_bytes([bytes[16], bytes[17]]);
            SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port)
        }
        _ => return None,
    };
    if is_routable(addr) {
        Some(addr)
    } else {
        None
    }
}

/// Разбирает список компактных адресов пиров.
fn decode_values(value: &Value) -> Vec<SocketAddr> {
    match value.as_list() {
        Some(items) => items.iter().filter_map(decode_compact_peer).collect(),
        None => Vec::new(),
    }
}

/// Разбирает входящее KRPC-сообщение.
fn decode_message(data: &[u8]) -> Result<KrpcMessage, DhtError> {
    let value = bencode::decode_all(data)
        .map_err(|error| DhtError::Protocol(format!("bencode: {error}")))?;
    let kind = value
        .get("y")
        .and_then(Value::as_bytes)
        .ok_or_else(|| DhtError::Protocol("отсутствует поле y".to_string()))?;
    let transaction = parse_transaction(value.get("t"))?;
    if kind == b"q" {
        let name = value
            .get("q")
            .and_then(Value::as_str)
            .ok_or_else(|| DhtError::Protocol("отсутствует имя запроса q".to_string()))?;
        let arguments = value
            .get("a")
            .ok_or_else(|| DhtError::Protocol("отсутствует словарь аргументов a".to_string()))?;
        let id = parse_id(arguments.get("id"), "id запроса")?;
        let query = parse_query_kind(name, arguments)?;
        return Ok(KrpcMessage::Query {
            transaction,
            query: KrpcQuery { id, kind: query },
        });
    }
    if kind == b"r" {
        let response = value
            .get("r")
            .ok_or_else(|| DhtError::Protocol("отсутствует словарь ответа r".to_string()))?;
        let id = parse_id(response.get("id"), "id ответа")?;
        let nodes = match response.get("nodes").and_then(Value::as_bytes) {
            Some(bytes) => decode_compact_nodes(bytes),
            None => Vec::new(),
        };
        let values = match response.get("values") {
            Some(list) => decode_values(list),
            None => Vec::new(),
        };
        let token = response
            .get("token")
            .and_then(Value::as_bytes)
            .map(<[u8]>::to_vec);
        return Ok(KrpcMessage::Response {
            transaction,
            response: KrpcResponse {
                id,
                nodes,
                values,
                token,
            },
        });
    }
    if kind == b"e" {
        return Ok(KrpcMessage::Error {
            transaction,
            message: parse_remote_error(&value),
        });
    }
    Err(DhtError::Protocol(format!(
        "неизвестный тип сообщения: {}",
        String::from_utf8_lossy(kind)
    )))
}

/// Разбирает вид запроса по его имени.
fn parse_query_kind(name: &str, arguments: &Value) -> Result<QueryKind, DhtError> {
    match name {
        "ping" => Ok(QueryKind::Ping),
        "find_node" => Ok(QueryKind::FindNode {
            target: parse_id(arguments.get("target"), "target")?,
        }),
        "get_peers" => Ok(QueryKind::GetPeers {
            info_hash: parse_id(arguments.get("info_hash"), "info_hash")?,
        }),
        "announce_peer" => {
            let info_hash = parse_id(arguments.get("info_hash"), "info_hash")?;
            let token = arguments
                .get("token")
                .and_then(Value::as_bytes)
                .ok_or_else(|| DhtError::Protocol("в announce_peer отсутствует token".to_string()))?
                .to_vec();
            let port = parse_port(arguments.get("port"))?;
            Ok(QueryKind::AnnouncePeer {
                info_hash,
                port,
                token,
            })
        }
        other => Err(DhtError::Protocol(format!("неизвестный запрос: {other}"))),
    }
}

/// Разбирает 20-байтовое поле.
fn parse_id(value: Option<&Value>, field: &str) -> Result<NodeId, DhtError> {
    let bytes = value
        .and_then(Value::as_bytes)
        .ok_or_else(|| DhtError::Protocol(format!("отсутствует или неверен {field}")))?;
    if bytes.len() != ID_LEN {
        return Err(DhtError::Protocol(format!(
            "{field}: ожидалось {ID_LEN} байт, получено {}",
            bytes.len()
        )));
    }
    let mut id = [0u8; ID_LEN];
    id.copy_from_slice(bytes);
    Ok(id)
}

/// Разбирает идентификатор транзакции.
fn parse_transaction(value: Option<&Value>) -> Result<u16, DhtError> {
    let bytes = value
        .and_then(Value::as_bytes)
        .ok_or_else(|| DhtError::Protocol("отсутствует идентификатор t".to_string()))?;
    match bytes {
        [high, low] => Ok(u16::from_be_bytes([*high, *low])),
        _ => Err(DhtError::Protocol(format!(
            "t: ожидалось 2 байта, получено {}",
            bytes.len()
        ))),
    }
}

/// Разбирает порт из аргументов запроса.
fn parse_port(value: Option<&Value>) -> Result<u16, DhtError> {
    let port = value.and_then(Value::as_int).unwrap_or(0);
    if (0..=i64::from(u16::MAX)).contains(&port) {
        Ok(port as u16)
    } else {
        Err(DhtError::Protocol(format!(
            "port: недопустимое значение {port}"
        )))
    }
}

/// Разбирает KRPC-ошибку в человекочитаемую строку.
fn parse_remote_error(value: &Value) -> String {
    let Some(list) = value.get("e").and_then(Value::as_list) else {
        return "узел вернул ошибку без описания".to_string();
    };
    let code = list.first().and_then(Value::as_int).unwrap_or(0);
    let text = list.get(1).and_then(Value::as_str).unwrap_or("без текста");
    format!("код {code}: {text}")
}

/// Токен `announce_peer`, привязанный к IP узла.
fn make_token(secret: &[u8; TOKEN_SECRET_LEN], ip: IpAddr) -> Vec<u8> {
    let mut hasher = Sha1::new();
    hasher.update(secret);
    match ip {
        IpAddr::V4(v4) => hasher.update(v4.octets()),
        IpAddr::V6(v6) => hasher.update(v6.octets()),
    }
    let digest = hasher.finalize();
    digest[..TOKEN_LEN].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Конфиг для сетевых тестов: всегда явный bootstrap без стандартных узлов.
    fn конфиг_для_теста(bootstrap: Vec<String>) -> DhtConfig {
        DhtConfig {
            port: 0,
            bootstrap,
            max_nodes: 64,
        }
    }

    /// Адрес на loopback с заданным портом.
    fn локальный_адрес(порт: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), порт)
    }

    /// Запускает узел, слушающий только loopback.
    async fn поднять_узел(bootstrap: Vec<String>) -> (Arc<Dht>, u16) {
        let dht = Arc::new(Dht::new(конфиг_для_теста(bootstrap)).bind_loopback());
        let port = dht.start().await.expect("узел должен запуститься");
        (dht, port)
    }

    /// Ждёт выполнения условия не дольше указанного срока.
    async fn дождаться<F>(mut условие: F, предел: Duration) -> bool
    where
        F: FnMut() -> bool,
    {
        let deadline = Instant::now() + предел;
        loop {
            if условие() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Принимает ответ с нужным id транзакции, пропуская прочие datagram.
    async fn принять_сообщение(
        сокет: &UdpSocket,
        ожидаемая_транзакция: u16,
    ) -> KrpcMessage {
        let mut буфер = [0u8; 4096];
        for _ in 0..8 {
            let (длина, _) = timeout(Duration::from_secs(2), сокет.recv_from(&mut буфер))
                .await
                .expect("нет ответа от узла DHT")
                .expect("ошибка приёма UDP");
            let сообщение =
                decode_message(&буфер[..длина]).expect("ответ должен быть валидным KRPC");
            let transaction = match &сообщение {
                KrpcMessage::Query { transaction, .. }
                | KrpcMessage::Response { transaction, .. }
                | KrpcMessage::Error { transaction, .. } => *transaction,
            };
            if transaction == ожидаемая_транзакция {
                return сообщение;
            }
        }
        panic!("не дождались ответа с транзакцией {ожидаемая_транзакция}");
    }

    // ---------- конфигурация и идентификаторы ----------

    #[test]
    fn конфиг_по_умолчанию_соответствует_контракту() {
        let config = DhtConfig::default();
        assert_eq!(config.port, 0);
        assert!(config.bootstrap.is_empty());
        assert_eq!(config.max_nodes, 1024);

        assert_eq!(DEFAULT_BOOTSTRAP.len(), 3);
        assert!(DEFAULT_BOOTSTRAP.contains(&"router.bittorrent.com:6881"));

        let stats = DhtStats::default();
        assert!(!stats.running);
        assert_eq!(stats.nodes, 0);
        assert_eq!(stats.peer_cache, 0);
        assert_eq!(stats.queries, 0);
        assert_eq!(stats.responses, 0);
    }

    #[test]
    fn идентификаторы_узлов_различаются() {
        let первый = Dht::new(DhtConfig::default());
        let второй = Dht::new(DhtConfig::default());
        assert_eq!(первый.node_id().len(), ID_LEN);
        assert_ne!(первый.node_id(), второй.node_id());
    }

    // ---------- кодирование запросов ----------

    #[test]
    fn кодирование_ping_запроса() {
        let id = [7u8; ID_LEN];
        let packet = encode_query(0x1234, &id, &QueryKind::Ping);
        let value = bencode::decode_all(&packet).expect("валидный bencode");
        assert_eq!(value.get("y").and_then(Value::as_str), Some("q"));
        assert_eq!(value.get("q").and_then(Value::as_str), Some("ping"));
        assert_eq!(
            value.get("t").and_then(Value::as_bytes),
            Some(&[0x12u8, 0x34][..])
        );
        let arguments = value.get("a").expect("должен быть словарь a");
        assert_eq!(arguments.get("id").and_then(Value::as_bytes), Some(&id[..]));
    }

    #[test]
    fn кодирование_get_peers_запроса() {
        let hash = [0xABu8; ID_LEN];
        let packet = encode_query(1, &[1u8; ID_LEN], &QueryKind::GetPeers { info_hash: hash });
        let value = bencode::decode_all(&packet).expect("валидный bencode");
        assert_eq!(value.get("q").and_then(Value::as_str), Some("get_peers"));
        assert!(value.as_dict().is_some());
        let arguments = value.get("a").expect("должен быть словарь a");
        assert_eq!(
            arguments.get("info_hash").and_then(Value::as_bytes),
            Some(&hash[..])
        );
    }

    #[test]
    fn кодирование_announce_peer_запроса() {
        let hash = [2u8; ID_LEN];
        let packet = encode_query(
            0xFFFF,
            &[3u8; ID_LEN],
            &QueryKind::AnnouncePeer {
                info_hash: hash,
                port: 6881,
                token: b"token".to_vec(),
            },
        );
        let value = bencode::decode_all(&packet).expect("валидный bencode");
        assert_eq!(
            value.get("q").and_then(Value::as_str),
            Some("announce_peer")
        );
        let arguments = value.get("a").expect("должен быть словарь a");
        assert_eq!(arguments.get("port").and_then(Value::as_int), Some(6881));
        assert_eq!(
            arguments.get("info_hash").and_then(Value::as_bytes),
            Some(&hash[..])
        );
        assert_eq!(
            arguments.get("token").and_then(Value::as_bytes),
            Some(&b"token"[..])
        );
    }

    // ---------- разбор ответов ----------

    #[test]
    fn разбор_ответа_на_ping() {
        let packet = bencode::encode(&bencode_dict(vec![
            (
                "r",
                bencode_dict(vec![("id", Value::Bytes(vec![9u8; ID_LEN]))]),
            ),
            ("t", Value::Bytes(vec![0x00, 0x2A])),
            ("y", Value::string("r")),
        ]));
        match decode_message(&packet).expect("валидный ответ") {
            KrpcMessage::Response {
                transaction,
                response,
            } => {
                assert_eq!(transaction, 0x002A);
                assert_eq!(response.id, [9u8; ID_LEN]);
                assert!(response.nodes.is_empty());
                assert!(response.values.is_empty());
                assert!(response.token.is_none());
            }
            _ => panic!("ожидался KRPC-ответ"),
        }
    }

    #[test]
    fn разбор_ответа_с_компактными_узлами() {
        let узел_1 = локальный_адрес(6901);
        let узел_2 = локальный_адрес(6902);
        let compact = encode_compact_nodes(&[(узел_1, [1u8; ID_LEN]), (узел_2, [2u8; ID_LEN])]);
        let packet = bencode::encode(&bencode_dict(vec![
            (
                "r",
                bencode_dict(vec![
                    ("id", Value::Bytes(vec![3u8; ID_LEN])),
                    ("nodes", Value::Bytes(compact)),
                ]),
            ),
            ("t", Value::Bytes(vec![0x01, 0x02])),
            ("y", Value::string("r")),
        ]));
        match decode_message(&packet).expect("валидный ответ") {
            KrpcMessage::Response { response, .. } => {
                assert_eq!(response.id, [3u8; ID_LEN]);
                assert_eq!(
                    response.nodes,
                    vec![(узел_1, [1u8; ID_LEN]), (узел_2, [2u8; ID_LEN])]
                );
                assert!(response.values.is_empty());
            }
            _ => panic!("ожидался KRPC-ответ"),
        }
    }

    #[test]
    fn разбор_ответа_со_значениями_пиров() {
        let packet = bencode::encode(&bencode_dict(vec![
            (
                "r",
                bencode_dict(vec![
                    ("id", Value::Bytes(vec![4u8; ID_LEN])),
                    (
                        "values",
                        Value::List(vec![
                            encode_compact_value(локальный_адрес(1111)),
                            encode_compact_value(локальный_адрес(2222)),
                        ]),
                    ),
                    ("token", Value::Bytes(b"tok".to_vec())),
                ]),
            ),
            ("t", Value::Bytes(vec![0xAB, 0xCD])),
            ("y", Value::string("r")),
        ]));
        match decode_message(&packet).expect("валидный ответ") {
            KrpcMessage::Response {
                transaction,
                response,
            } => {
                assert_eq!(transaction, 0xABCD);
                assert_eq!(
                    response.values,
                    vec![локальный_адрес(1111), локальный_адрес(2222)]
                );
                assert_eq!(response.token.as_deref(), Some(&b"tok"[..]));
                assert!(response.nodes.is_empty());
            }
            _ => panic!("ожидался KRPC-ответ"),
        }
    }

    #[test]
    fn компактные_узлы_кодируются_и_разбираются() {
        let узлы = vec![
            (локальный_адрес(6881), [0x11u8; ID_LEN]),
            (локальный_адрес(6882), [0x22u8; ID_LEN]),
        ];
        let compact = encode_compact_nodes(&узлы);
        assert_eq!(compact.len(), 2 * COMPACT_NODE_LEN);
        assert_eq!(decode_compact_nodes(&compact), узлы);

        // Хвостовые байты и неполный узел отбрасываются без паники.
        let mut с_хвостом = compact.clone();
        с_хвостом.push(0xEE);
        assert_eq!(decode_compact_nodes(&с_хвостом), узлы);
        assert!(decode_compact_nodes(&[0x01, 0x02, 0x03]).is_empty());

        // Порт 0 не попадает в таблицу (у обоих узлов).
        let mut нулевые_порты = compact;
        for основа in [0usize, COMPACT_NODE_LEN] {
            нулевые_порты[основа + 24] = 0;
            нулевые_порты[основа + 25] = 0;
        }
        assert!(decode_compact_nodes(&нулевые_порты).is_empty());
    }

    #[test]
    fn компактные_пиры_игнорируют_мусор() {
        let mut ipv6 = vec![0u8; COMPACT_PEER_V6_LEN];
        ipv6[15] = 1;
        ipv6[16] = 0x1A;
        ipv6[17] = 0xE1;
        let value = Value::List(vec![
            encode_compact_value(локальный_адрес(6881)),
            Value::Bytes(vec![1, 2, 3]),
            Value::Bytes(vec![127, 0, 0, 1, 0, 0]),
            Value::Bytes(ipv6),
            Value::Int(42),
        ]);
        let пиры = decode_values(&value);
        assert_eq!(пиры.len(), 2);
        assert_eq!(пиры[0], локальный_адрес(6881));
        assert_eq!(пиры[1].port(), 6881);
        assert!(пиры[1].is_ipv6());
        assert!(decode_values(&Value::Int(1)).is_empty());
    }

    #[test]
    fn разбор_krpc_ошибки() {
        let packet = encode_error(0x0102, 201, "неверный токен");
        match decode_message(&packet).expect("валидная ошибка") {
            KrpcMessage::Error {
                transaction,
                message,
            } => {
                assert_eq!(transaction, 0x0102);
                assert!(message.contains("201"), "сообщение: {message}");
                assert!(message.contains("неверный токен"), "сообщение: {message}");
            }
            _ => panic!("ожидалась KRPC-ошибка"),
        }
    }

    #[test]
    fn мусорные_пакеты_отбрасываются() {
        let без_y = bencode::encode(&bencode_dict(vec![("t", Value::Bytes(vec![0, 1]))]));
        let без_t = bencode::encode(&bencode_dict(vec![
            ("y", Value::string("r")),
            ("r", bencode_dict(vec![])),
        ]));
        let длинный_t = bencode::encode(&bencode_dict(vec![
            ("t", Value::Bytes(vec![0, 1, 2])),
            ("y", Value::string("r")),
            (
                "r",
                bencode_dict(vec![("id", Value::Bytes(vec![0u8; ID_LEN]))]),
            ),
        ]));
        let неизвестный_запрос = bencode::encode(&bencode_dict(vec![
            (
                "a",
                bencode_dict(vec![("id", Value::Bytes(vec![0u8; ID_LEN]))]),
            ),
            ("q", Value::string("unknown")),
            ("t", Value::Bytes(vec![0, 1])),
            ("y", Value::string("q")),
        ]));
        let ответ_без_id = bencode::encode(&bencode_dict(vec![
            ("r", bencode_dict(vec![])),
            ("t", Value::Bytes(vec![0, 1])),
            ("y", Value::string("r")),
        ]));
        let битый_bencode = b"d1:a".to_vec();
        let не_словарь = bencode::encode(&Value::Int(5));

        for пакет in [
            без_y,
            без_t,
            длинный_t,
            неизвестный_запрос,
            ответ_без_id,
            битый_bencode,
            не_словарь,
            Vec::new(),
            vec![0xFF, 0xFE, 0x00],
        ] {
            assert!(
                decode_message(&пакет).is_err(),
                "мусор не должен разбираться: {пакет:?}"
            );
        }
    }

    // ---------- таблица маршрутизации ----------

    #[test]
    fn таблица_ограничивает_бакет_k_узлами() {
        let mut таблица = RoutingTable::new([0u8; ID_LEN], 128);
        let mut добавлено = 0;
        for байт in 0x80u8..=0x88 {
            let mut id = [0u8; ID_LEN];
            id[ID_LEN - 1] = байт;
            if таблица.insert(локальный_адрес(u16::from(байт) + 1), id) {
                добавлено += 1;
            }
        }
        assert_eq!(добавлено, BUCKET_SIZE);
        assert_eq!(таблица.len(), BUCKET_SIZE);
    }

    #[test]
    fn таблица_находит_ближайшие_узлы() {
        let mut таблица = RoutingTable::new([0u8; ID_LEN], 128);
        for (последний, порт) in [(0x10u8, 1u16), (0x01, 2), (0x40, 3)] {
            let mut id = [0u8; ID_LEN];
            id[ID_LEN - 1] = последний;
            assert!(таблица.insert(локальный_адрес(порт), id));
        }
        let ближайшие = таблица.closest(&[0u8; ID_LEN], 2);
        assert_eq!(ближайшие.len(), 2);
        assert_eq!(ближайшие[0].id[ID_LEN - 1], 0x01);
        assert_eq!(ближайшие[1].id[ID_LEN - 1], 0x10);
    }

    #[test]
    fn таблица_обновляет_известный_узел() {
        let mut таблица = RoutingTable::new([0u8; ID_LEN], 128);
        let mut id = [0u8; ID_LEN];
        id[0] = 1;
        assert!(таблица.insert(локальный_адрес(1000), id));
        assert!(таблица.insert(локальный_адрес(2000), id));
        assert_eq!(таблица.len(), 1);
        assert!(таблица.contains_addr(локальный_адрес(2000)));
        assert!(!таблица.contains_addr(локальный_адрес(1000)));
    }

    #[test]
    fn таблица_вытесняет_мёртвый_узел() {
        let mut таблица = RoutingTable::new([0u8; ID_LEN], 128);
        for байт in 0x80u8..=0x87 {
            let mut id = [0u8; ID_LEN];
            id[ID_LEN - 1] = байт;
            assert!(таблица.insert(локальный_адрес(u16::from(байт)), id));
        }
        let мёртвый = локальный_адрес(0x80);
        таблица.mark_failure(мёртвый);
        таблица.mark_failure(мёртвый);

        let mut новый_id = [0u8; ID_LEN];
        новый_id[ID_LEN - 1] = 0x88;
        assert!(таблица.insert(локальный_адрес(0x88), новый_id));
        assert_eq!(таблица.len(), BUCKET_SIZE);
        assert!(таблица.contains_addr(локальный_адрес(0x88)));
        assert!(!таблица.contains_addr(мёртвый));
    }

    #[test]
    fn таблица_удаляет_мёртвые_узлы() {
        let mut таблица = RoutingTable::new([0u8; ID_LEN], 128);
        let mut id = [0u8; ID_LEN];
        id[0] = 0x10;
        assert!(таблица.insert(локальный_адрес(7000), id));
        таблица.mark_failure(локальный_адрес(7000));
        таблица.mark_failure(локальный_адрес(7000));
        assert_eq!(таблица.remove_dead(), 1);
        assert_eq!(таблица.len(), 0);
    }

    #[test]
    fn таблица_соблюдает_ограничение_max_nodes() {
        let mut таблица = RoutingTable::new([0u8; ID_LEN], 2);
        let mut id_a = [0u8; ID_LEN];
        id_a[ID_LEN - 1] = 0x80;
        let mut id_b = [0u8; ID_LEN];
        id_b[ID_LEN - 1] = 0x01;
        let mut id_c = [0u8; ID_LEN];
        id_c[ID_LEN - 1] = 0x40;
        assert!(таблица.insert(локальный_адрес(1), id_a));
        assert!(таблица.insert(локальный_адрес(2), id_b));
        assert!(!таблица.insert(локальный_адрес(3), id_c));
        assert_eq!(таблица.len(), 2);
    }

    // ---------- кэш пиров ----------

    #[test]
    fn кэш_пиров_фильтрует_и_дедуплицирует() {
        let mut кэш = PeerCache::default();
        let hash = InfoHash::new([1u8; ID_LEN]);
        кэш.insert(hash, локальный_адрес(6881));
        кэш.insert(hash, локальный_адрес(6881));
        assert_eq!(кэш.len(), 1);

        кэш.insert(hash, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0));
        assert_eq!(кэш.len(), 1);
        кэш.insert(hash, SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 80));
        assert_eq!(кэш.len(), 1);

        кэш.insert(hash, локальный_адрес(6882));
        assert_eq!(кэш.len(), 2);
        assert_eq!(
            кэш.peek(&hash, 10),
            vec![локальный_адрес(6881), локальный_адрес(6882)]
        );
    }

    #[test]
    fn кэш_пиров_ограничивает_размер() {
        let mut кэш = PeerCache::default();
        let hash = InfoHash::new([2u8; ID_LEN]);
        for порт in 1..=PEERS_PER_HASH_LIMIT as u16 + 10 {
            кэш.insert(hash, локальный_адрес(порт));
        }
        assert_eq!(кэш.len(), PEERS_PER_HASH_LIMIT);
    }

    #[test]
    fn кэш_пиров_удаляет_устаревшие() {
        let mut кэш = PeerCache::default();
        кэш.insert(InfoHash::new([3u8; ID_LEN]), локальный_адрес(6881));
        кэш.prune(Duration::ZERO);
        assert_eq!(кэш.len(), 0);
    }

    #[test]
    fn take_peers_очищает_кэш() {
        let mut кэш = PeerCache::default();
        let hash = InfoHash::new([4u8; ID_LEN]);
        кэш.insert(hash, локальный_адрес(6881));
        assert_eq!(кэш.take(&hash), vec![локальный_адрес(6881)]);
        assert!(кэш.take(&hash).is_empty());
        assert_eq!(кэш.len(), 0);
    }

    // ---------- токены и статистика ----------

    #[test]
    fn токен_привязан_к_ip() {
        let secret = [0x5Au8; TOKEN_SECRET_LEN];
        let токен = make_token(&secret, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(токен.len(), TOKEN_LEN);
        assert_eq!(токен, make_token(&secret, IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_ne!(
            токен,
            make_token(&secret, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)))
        );
        assert_ne!(
            токен,
            make_token(&[0x5Bu8; TOKEN_SECRET_LEN], IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
    }

    #[test]
    fn статистика_до_запуска() {
        let dht = Dht::new(конфиг_для_теста(vec![]));
        let stats = dht.stats();
        assert!(!stats.running);
        assert_eq!(stats.nodes, 0);
        assert_eq!(stats.peer_cache, 0);
        assert_eq!(stats.queries, 0);
        assert_eq!(stats.responses, 0);
        assert!(dht.take_peers(&InfoHash::new([0u8; ID_LEN])).is_empty());
    }

    // ---------- сетевые тесты на 127.0.0.1 ----------

    #[tokio::test]
    async fn повторный_start_возвращает_тот_же_порт() {
        let (dht, первый_порт) = поднять_узел(vec!["127.0.0.1:9".to_string()]).await;
        let второй_порт = dht.start().await.expect("повторный старт");
        assert_eq!(первый_порт, второй_порт);
        assert!(первый_порт > 0);
        assert!(dht.stats().running);
        dht.shutdown().await;
    }

    #[tokio::test]
    async fn announce_после_остановки_возвращает_closed() {
        let (dht, _порт) = поднять_узел(vec!["127.0.0.1:9".to_string()]).await;
        assert!(dht.stats().running);
        dht.shutdown().await;
        assert!(!dht.stats().running);

        let error = dht
            .announce(InfoHash::new([3u8; ID_LEN]), 6881)
            .await
            .expect_err("после остановки announce невозможен");
        assert!(matches!(error, DhtError::Closed));
    }

    #[tokio::test]
    async fn announce_без_узлов_в_таблице_возвращает_timeout() {
        let (dht, _порт) = поднять_узел(vec!["127.0.0.1:9".to_string()]).await;
        let error = dht
            .announce(InfoHash::new([5u8; ID_LEN]), 6881)
            .await
            .expect_err("без узлов announce должен упасть");
        assert!(matches!(error, DhtError::Timeout));
        dht.shutdown().await;
    }

    #[tokio::test]
    async fn два_узла_находят_друг_друга() {
        let (первый, порт_первого) = поднять_узел(vec!["127.0.0.1:9".to_string()]).await;
        let (второй, _) = поднять_узел(vec![локальный_адрес(порт_первого).to_string()]).await;

        assert!(
            дождаться(|| второй.stats().nodes > 0, Duration::from_secs(5)).await,
            "второй узел не добавил bootstrap-узел"
        );
        assert!(
            дождаться(|| первый.stats().nodes > 0, Duration::from_secs(5)).await,
            "первый узел не узнал о втором"
        );

        let stats = второй.stats();
        assert!(stats.queries > 0, "второй узел не отправил запросов");
        assert!(
            stats.responses > 0,
            "второй узел не получил ответов: {stats:?}"
        );

        первый.shutdown().await;
        второй.shutdown().await;
    }

    #[tokio::test]
    async fn announce_и_take_peers_на_локальной_сети() {
        let (сервер, порт_сервера) = поднять_узел(vec!["127.0.0.1:9".to_string()]).await;
        let (клиент, _) = поднять_узел(vec![локальный_адрес(порт_сервера).to_string()]).await;
        assert!(
            дождаться(|| клиент.stats().nodes > 0, Duration::from_secs(5)).await,
            "клиент не увидел сервер"
        );

        let hash = InfoHash::new([0x42u8; ID_LEN]);
        let порт_пира = 51_413u16;
        клиент
            .announce(hash, порт_пира)
            .await
            .expect("первый announce должен пройти");
        assert!(
            дождаться(|| сервер.stats().peer_cache > 0, Duration::from_secs(5)).await,
            "announce_peer не дошёл до сервера"
        );

        клиент
            .announce(hash, порт_пира)
            .await
            .expect("второй announce должен пройти");
        let пиры = клиент.take_peers(&hash);
        assert!(
            пиры.contains(&локальный_адрес(порт_пира)),
            "в кэше нет анонсированного пира: {пиры:?}"
        );
        assert!(
            клиент.take_peers(&hash).is_empty(),
            "take_peers должен очищать кэш"
        );

        сервер.shutdown().await;
        клиент.shutdown().await;
    }

    #[tokio::test]
    async fn announce_опрашивает_узлы_из_ответа_и_собирает_их_пиров() {
        // Ради чего нужны несколько раундов `get_peers`: адрес узла, которого в
        // таблице не было на первом раунде, приходит только в ответе. Скрытый
        // узел появляется у сервера уже после bootstrap клиента, поэтому дойти
        // до него можно лишь следующим раундом.
        let (сервер, порт_сервера) = поднять_узел(vec!["127.0.0.1:9".to_string()]).await;
        let (клиент, _) = поднять_узел(vec![локальный_адрес(порт_сервера).to_string()]).await;
        assert!(
            дождаться(|| клиент.stats().nodes > 0, Duration::from_secs(5)).await,
            "клиент не увидел сервер"
        );

        // Скрытый узел — сырой сокет, который умеет только отвечать на запросы.
        let скрытый = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("сокет скрытого узла");
        let адрес_скрытого = скрытый.local_addr().expect("локальный адрес скрытого узла");
        let id_скрытого = [0x6Bu8; ID_LEN];
        // Пинг — единственный способ попасть в таблицу сервера: клиент об этом
        // адресе до announce ничего не знает.
        let транзакция_пинга = 0x7777u16;
        скрытый
            .send_to(
                &encode_query(транзакция_пинга, &id_скрытого, &QueryKind::Ping),
                локальный_адрес(порт_сервера),
            )
            .await
            .expect("пинг скрытого узла");
        принять_сообщение(&скрытый, транзакция_пинга).await;
        assert!(
            !клиент.contains_node(адрес_скрытого),
            "клиент не должен знать скрытый узел до announce"
        );

        let hash = InfoHash::new([0x6Cu8; ID_LEN]);
        let пир_скрытого = локальный_адрес(51_415);
        let анонс = {
            let узел = Arc::clone(&клиент);
            tokio::spawn(async move { узел.announce(hash, 51_414).await })
        };

        // Второй раунд обязан прийти на скрытый узел: отвечаем и отдаём пира.
        let mut буфер = [0u8; 4096];
        let (длина, откуда) = timeout(Duration::from_secs(5), скрытый.recv_from(&mut буфер))
            .await
            .expect("клиент не опросил узел из ответа")
            .expect("ошибка приёма UDP");
        assert!(откуда.ip().is_loopback(), "запрос пришёл не с loopback");
        let сообщение = decode_message(&буфер[..длина]).expect("валидный KRPC");
        let KrpcMessage::Query { transaction, query } = сообщение else {
            panic!("ожидался get_peers, пришло: {сообщение:?}");
        };
        assert!(
            matches!(query.kind, QueryKind::GetPeers { .. }),
            "скрытому узлу пришёл не get_peers"
        );
        скрытый
            .send_to(
                &encode_response(
                    transaction,
                    &id_скрытого,
                    &ResponseBody::GetPeers {
                        token: vec![7u8; TOKEN_LEN],
                        values: vec![пир_скрытого],
                        nodes: Vec::new(),
                    },
                ),
                откуда,
            )
            .await
            .expect("ответ скрытого узла");

        анонс
            .await
            .expect("задача announce не должна паниковать")
            .expect("announce должен пройти");

        let пиры = клиент.take_peers(&hash);
        assert!(
            пиры.contains(&пир_скрытого),
            "пир узла, найденного во втором раунде, не попал в кэш: {пиры:?}"
        );

        сервер.shutdown().await;
        клиент.shutdown().await;
    }

    #[tokio::test]
    async fn сырой_клиент_получает_токен_и_анонсирует_пира() {
        let (dht, порт_узла) = поднять_узел(vec!["127.0.0.1:9".to_string()]).await;
        let адрес_узла = локальный_адрес(порт_узла);
        let сокет = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("UDP-сокет для теста");
        let hash = [0x77u8; ID_LEN];

        let запрос = encode_query(
            0x0001,
            &[0x11u8; ID_LEN],
            &QueryKind::GetPeers { info_hash: hash },
        );
        сокет
            .send_to(&запрос, адрес_узла)
            .await
            .expect("отправка get_peers");
        let токен = match принять_сообщение(&сокет, 0x0001).await {
            KrpcMessage::Response { response, .. } => response
                .token
                .expect("в ответе get_peers должен быть token"),
            _ => panic!("ожидался ответ на get_peers"),
        };
        assert_eq!(токен.len(), TOKEN_LEN);

        let анонс = encode_query(
            0x0002,
            &[0x11u8; ID_LEN],
            &QueryKind::AnnouncePeer {
                info_hash: hash,
                port: 51_000,
                token: токен,
            },
        );
        сокет
            .send_to(&анонс, адрес_узла)
            .await
            .expect("отправка announce_peer");
        let _ = принять_сообщение(&сокет, 0x0002).await;
        assert!(
            дождаться(|| dht.stats().peer_cache == 1, Duration::from_secs(2)).await,
            "announce_peer не сохранил пира"
        );

        let повторный = encode_query(
            0x0003,
            &[0x11u8; ID_LEN],
            &QueryKind::GetPeers { info_hash: hash },
        );
        сокет
            .send_to(&повторный, адрес_узла)
            .await
            .expect("повторная отправка get_peers");
        match принять_сообщение(&сокет, 0x0003).await {
            KrpcMessage::Response { response, .. } => {
                assert!(
                    response.values.contains(&локальный_адрес(51_000)),
                    "в values нет анонсированного пира: {:?}",
                    response.values
                );
                assert!(response.nodes.is_empty());
            }
            _ => panic!("ожидался ответ на get_peers"),
        }

        dht.shutdown().await;
    }
}
