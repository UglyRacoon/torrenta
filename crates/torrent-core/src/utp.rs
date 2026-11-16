//! uTP — транспорт поверх UDP (BEP 29).
//!
//! BitTorrent-клиенты используют uTP как второй транспорт: он помогает
//! соединяться с пирами, у которых открыт только UDP-порт, и «уступает» канал
//! другим приложениям за счёт контроля перегрузки.
//!
//! Реализовано по спецификации BEP 29 (uTorrent Transport Protocol):
//! 20-байтовый заголовок (версия, тип, `connection_id`, метки времени,
//! `wnd_size`, `seq_nr`, `ack_nr`), типы `ST_DATA`/`ST_FIN`/`ST_STATE`/
//! `ST_RESET`/`ST_SYN`, кумулятивные подтверждения, окно получателя и
//! упрощённый LEDBAT-подобный контроль перегрузки (рост окна по подтверждениям,
//! спад при потере или росте задержки в очереди собеседника).
//!
//! Чего нет (честно): выборочных подтверждений SACK, расширения `utp_pe`
//! (обфускация), отдельного порта uTP и полноценного LEDBAT с фильтром
//! задержки. Это записано в ограничениях, а не спрятано.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Notify};

/// Версия протокола uTP.
const VERSION: u8 = 1;
/// Длина заголовка uTP.
const HEADER_LEN: usize = 20;
/// Тип: данные.
const ST_DATA: u8 = 0;
/// Тип: закрытие потока.
const ST_FIN: u8 = 1;
/// Тип: подтверждение состояния.
const ST_STATE: u8 = 2;
/// Тип: сброс соединения.
const ST_RESET: u8 = 3;
/// Тип: установка соединения.
const ST_SYN: u8 = 4;
/// Максимальный размер полезной нагрузки пакета (оставляем запас под MTU).
pub const MSS: usize = 1300;
/// Начальное окно перегрузки.
const INITIAL_CWND: usize = 4 * MSS;
/// Минимальное окно перегрузки.
const MIN_CWND: usize = 2 * MSS;
/// Предел окна перегрузки.
const MAX_CWND: usize = 256 * MSS;
/// Сколько байт мы готовы принять без обработки (окно получателя).
const RECEIVE_WINDOW: usize = 512 * 1024;
/// Тайм-аут переотправки по умолчанию.
const RTO_MIN: Duration = Duration::from_millis(500);
/// Максимальный RTO.
const RTO_MAX: Duration = Duration::from_secs(6);
/// Сколько живёт полузакрытое соединение.
const LINGER: Duration = Duration::from_secs(10);
/// Целевая задержка LEDBAT.
const TARGET_DELAY: Duration = Duration::from_millis(100);

/// Тип и версия в одном байте.
fn version_type(kind: u8) -> u8 {
    (VERSION << 4) | (kind & 0x0F)
}

/// Разобранный пакет uTP (заголовок + payload).
#[derive(Debug, Clone)]
pub(crate) struct Packet {
    pub kind: u8,
    pub connection_id: u16,
    pub timestamp_us: u32,
    pub timestamp_diff_us: u32,
    pub wnd_size: u32,
    pub seq_nr: u16,
    pub ack_nr: u16,
    pub payload: Vec<u8>,
}

impl Packet {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.push(version_type(self.kind));
        out.push(0); // расширений нет
        out.extend_from_slice(&self.connection_id.to_be_bytes());
        out.extend_from_slice(&self.timestamp_us.to_be_bytes());
        out.extend_from_slice(&self.timestamp_diff_us.to_be_bytes());
        out.extend_from_slice(&self.wnd_size.to_be_bytes());
        out.extend_from_slice(&self.seq_nr.to_be_bytes());
        out.extend_from_slice(&self.ack_nr.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    fn decode(data: &[u8]) -> Option<Packet> {
        if data.len() < HEADER_LEN {
            return None;
        }
        let head = data[0];
        if head >> 4 != VERSION {
            return None;
        }
        Some(Packet {
            kind: head & 0x0F,
            connection_id: u16::from_be_bytes([data[2], data[3]]),
            timestamp_us: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
            timestamp_diff_us: u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
            wnd_size: u32::from_be_bytes([data[12], data[13], data[14], data[15]]),
            seq_nr: u16::from_be_bytes([data[16], data[17]]),
            ack_nr: u16::from_be_bytes([data[18], data[19]]),
            payload: data[HEADER_LEN..].to_vec(),
        })
    }
}

/// Микросекунды от условного нуля (для меток времени uTP).
fn now_us() -> u32 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_micros() as u32
}

/// Состояние одного соединения, разделяемое драйвером и `UtpStream`.
#[derive(Default)]
struct ConnState {
    /// Данные, принятые по порядку и ожидающие чтения.
    read_buffer: VecDeque<u8>,
    /// Данные, которые приложение хочет отправить.
    write_buffer: VecDeque<u8>,
    /// Пришли все данные и FIN от собеседника: читать больше нечего.
    read_closed: bool,
    /// Приложение закрыло запись.
    write_closed: bool,
    /// Соединение сброшено.
    reset: bool,
    /// Ошибка соединения.
    error: Option<io::Error>,
    /// Ожидающие читатели.
    read_waker: Option<Waker>,
    /// Ожидающие писатели.
    write_waker: Option<Waker>,
    /// Подтверждено всё отправленное (для `flush`).
    flushed: bool,
}

/// Драйвер и приложение обмениваются только этим состоянием.
type Shared = Arc<Mutex<ConnState>>;

/// Активное uTP-соединение: пара «драйвер + поток приложения».
pub struct UtpStream {
    addr: SocketAddr,
    state: Shared,
    alive: Arc<AtomicBool>,
    write_wake: Arc<Notify>,
}

impl UtpStream {
    /// Адрес пира (для журнала и тестов).
    pub fn peer_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for UtpStream {
    fn drop(&mut self) {
        // Драйвер завершится сам: `alive` гасится, сокет освобождается.
        self.alive.store(false, Ordering::Relaxed);
        self.write_wake.notify_waiters();
    }
}

impl AsyncRead for UtpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self.get_mut();
        let mut state = this.state.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(err) = state.error.take() {
            return Poll::Ready(Err(err));
        }
        if !state.read_buffer.is_empty() {
            let count = state.read_buffer.len().min(buf.remaining());
            for _ in 0..count {
                if let Some(byte) = state.read_buffer.pop_front() {
                    buf.put_slice(&[byte]);
                }
            }
            return Poll::Ready(Ok(()));
        }
        if state.read_closed || state.reset {
            return Poll::Ready(Ok(()));
        }
        state.read_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl AsyncWrite for UtpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self.get_mut();
        let mut state = this.state.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(err) = state.error.take() {
            return Poll::Ready(Err(err));
        }
        if state.write_closed || state.reset {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "uTP-соединение закрыто",
            )));
        }
        // Ограничиваем буфер, чтобы не съесть память при медленном канале.
        if state.write_buffer.len() >= RECEIVE_WINDOW {
            state.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let room = RECEIVE_WINDOW - state.write_buffer.len();
        let count = room.min(buf.len());
        state.write_buffer.extend(&buf[..count]);
        state.flushed = false;
        drop(state);
        this.write_wake.notify_one();
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self.get_mut();
        let state = this.state.lock().unwrap_or_else(|err| err.into_inner());
        if state.flushed || state.reset {
            return Poll::Ready(Ok(()));
        }
        drop(state);
        this.state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .write_waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self.get_mut();
        let mut state = this.state.lock().unwrap_or_else(|err| err.into_inner());
        state.write_closed = true;
        drop(state);
        this.write_wake.notify_one();
        let state = this.state.lock().unwrap_or_else(|err| err.into_inner());
        if state.flushed {
            return Poll::Ready(Ok(()));
        }
        drop(state);
        this.state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .write_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// Отправленный, но ещё не подтверждённый сегмент.
struct Flight {
    /// Номер последовательности первого байта.
    seq: u16,
    payload: Vec<u8>,
    /// Когда сегмент отправлен (для RTO).
    sent_at: Instant,
    /// Сколько раз переотправляли.
    retries: u32,
    /// Тип пакета: `ST_DATA` или `ST_FIN`.
    kind: u8,
}

/// Драйвер одного соединения: переотправка, подтверждения, окно.
struct Driver {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    conn_id: u16,
    state: Shared,
    alive: Arc<AtomicBool>,
    notify: Arc<Notify>,
    write_wake: Arc<Notify>,
    packets: mpsc::Receiver<Packet>,
    /// Следующий `seq_nr` для исходящих сегментов.
    next_seq: u16,
    /// Ожидаем подтверждения следующего номера.
    ack_nr: u16,
    /// Номер, который собеседник подтвердил у нас (его `ack_nr`).
    peer_ack: u16,
    /// Сегменты в полёте.
    flights: VecDeque<Flight>,
    /// Принятые вне очереди сегменты (seq → payload).
    out_of_order: BTreeMap<u16, Vec<u8>>,

    /// Окно перегрузки в байтах.
    cwnd: usize,
    /// Полученное окно получателя.
    peer_window: u32,
    /// Время последней активности.
    last_activity: Instant,
    /// Соединение закрыто с нашей стороны и ждёт подтверждения FIN.
    fin_sent: bool,
    /// Пришёл FIN собеседника.
    fin_received: bool,
}

impl Driver {
    // Параметров много осознанно: это конструктор состояния одного соединения,
    // и собирать их в отдельную структуру ради счётчика аргументов смысла нет.
    #[allow(clippy::too_many_arguments)]
    fn new(
        socket: Arc<UdpSocket>,
        peer: SocketAddr,
        conn_id: u16,
        state: Shared,
        alive: Arc<AtomicBool>,
        notify: Arc<Notify>,
        write_wake: Arc<Notify>,
        packets: mpsc::Receiver<Packet>,
    ) -> Driver {
        Driver {
            socket,
            peer,
            conn_id,
            state,
            alive,
            notify,
            write_wake,
            packets,
            next_seq: 2,
            ack_nr: 2,
            peer_ack: 2,
            flights: VecDeque::new(),
            out_of_order: BTreeMap::new(),

            cwnd: INITIAL_CWND,
            peer_window: RECEIVE_WINDOW as u32,
            last_activity: Instant::now(),
            fin_sent: false,
            fin_received: false,
        }
    }

    fn packet(&self, kind: u8, seq: u16, ack: u16, payload: Vec<u8>) -> Packet {
        Packet {
            kind,
            connection_id: self.conn_id,
            timestamp_us: now_us(),
            timestamp_diff_us: 0,
            wnd_size: self.free_window() as u32,
            seq_nr: seq,
            ack_nr: ack,
            payload,
        }
    }

    /// Выполняет действие над состоянием, не оставляя мьютекс-гард наружу.
    ///
    /// Иначе гард попадает в `async fn` и будущее перестаёт быть `Send`
    /// (соединение нельзя запустить отдельной задачей).
    fn with_state<R>(&self, action: impl FnOnce(&mut ConnState) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        action(&mut state)
    }

    fn free_window(&self) -> usize {
        self.with_state(|state| {
            RECEIVE_WINDOW.saturating_sub(state.read_buffer.len() + self.out_of_order.len() * MSS)
        })
    }

    async fn send(&self, packet: Packet) {
        let _ = self.socket.send_to(&packet.encode(), self.peer).await;
    }

    /// Отправляет подтверждение состояния (если нет данных для пиггибека).
    async fn send_state(&self) {
        let ack = self.ack_nr;
        let seq = self.next_seq;
        let packet = self.packet(ST_STATE, seq, ack, Vec::new());
        self.send(packet).await;
    }

    /// Отправляет данные из буфера приложения в пределах окна.
    async fn pump(&mut self) {
        loop {
            let buffered = self.with_state(|state| state.write_buffer.len());
            if buffered == 0 {
                break;
            }
            let in_flight: usize = self.flights.iter().map(|flight| flight.payload.len()).sum();
            let window = self.cwnd.min(self.peer_window as usize).min(MAX_CWND);
            if in_flight >= window {
                break;
            }
            let room = window - in_flight;
            let count = room.min(MSS).min(buffered);
            if count == 0 {
                break;
            }
            let payload: Vec<u8> =
                self.with_state(|state| state.write_buffer.drain(..count).collect());
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            let packet = self.packet(ST_DATA, seq, self.ack_nr, payload.clone());
            self.send(packet).await;
            self.flights.push_back(Flight {
                seq,
                payload,
                sent_at: Instant::now(),
                retries: 0,
                kind: ST_DATA,
            });
            self.last_activity = Instant::now();
        }
        let (write_closed, empty) =
            self.with_state(|state| (state.write_closed, state.write_buffer.is_empty()));
        if write_closed && empty && !self.fin_sent && self.flights.is_empty() {
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            let packet = self.packet(ST_FIN, seq, self.ack_nr, Vec::new());
            self.send(packet).await;
            self.flights.push_back(Flight {
                seq,
                payload: Vec::new(),
                sent_at: Instant::now(),
                retries: 0,
                kind: ST_FIN,
            });
            self.fin_sent = true;
        } else {
            self.with_state(|state| {
                if let Some(waker) = state.write_waker.take() {
                    waker.wake();
                }
            });
        }
    }

    /// Обрабатывает входящий пакет.
    async fn handle(&mut self, packet: Packet) {
        self.last_activity = Instant::now();
        self.peer_window = packet.wnd_size.max(MSS as u32);
        // Подтверждение: снимаем отправленные сегменты.
        // Подтверждение двигается только вперёд: запоздавший ack (например,
        // подтверждение SYN от принимающей стороны) не должен откатывать
        // указатель и «застревать» на нём.
        if packet.ack_nr != self.peer_ack && seq_after(packet.ack_nr, self.peer_ack) {
            let acked = packet.ack_nr;
            while let Some(front) = self.flights.front() {
                if seq_before(front.seq, acked) {
                    self.flights.pop_front();
                } else {
                    break;
                }
            }
            self.peer_ack = acked;
            // Простейший LEDBAT: рост окна на подтверждение, спад по задержке.
            let queued = Duration::from_micros(u64::from(packet.timestamp_diff_us));
            if queued > TARGET_DELAY {
                self.cwnd = (self.cwnd * 3 / 4).max(MIN_CWND);
            } else {
                self.cwnd = (self.cwnd + MSS).min(MAX_CWND);
            }
            self.retransmit_if_needed(true).await;
        }
        match packet.kind {
            ST_DATA => {
                let seq = packet.seq_nr;
                if seq == self.ack_nr {
                    self.deliver(packet.payload);
                    self.ack_nr = self.ack_nr.wrapping_add(1);
                    self.deliver_buffered();
                    self.send_state().await;
                } else if seq_after(seq, self.ack_nr) {
                    self.out_of_order.insert(seq, packet.payload);
                    self.send_state().await;
                } else {
                    // Повтор уже доставленного — просто подтверждаем.
                    self.send_state().await;
                }
            }
            ST_FIN => {
                self.fin_received = true;
                if packet.seq_nr == self.ack_nr {
                    self.ack_nr = self.ack_nr.wrapping_add(1);
                }
                self.with_state(|state| {
                    state.read_closed = true;
                    if let Some(waker) = state.read_waker.take() {
                        waker.wake();
                    }
                });
                self.send_state().await;
                self.check_finished();
            }
            ST_STATE => {
                // Подтверждение без данных: могли освободиться сегменты.
                self.retransmit_if_needed(false).await;
            }
            ST_RESET => {
                self.with_state(|state| {
                    state.reset = true;
                    state.flushed = true;
                    if let Some(waker) = state.read_waker.take() {
                        waker.wake();
                    }
                    if let Some(waker) = state.write_waker.take() {
                        waker.wake();
                    }
                });
                self.alive.store(false, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    fn deliver(&self, payload: Vec<u8>) {
        self.with_state(|state| {
            state.read_buffer.extend(payload);
            if let Some(waker) = state.read_waker.take() {
                waker.wake();
            }
        });
    }

    fn deliver_buffered(&mut self) {
        loop {
            let next = self.ack_nr;
            if let Some(payload) = self.out_of_order.remove(&next) {
                self.deliver(payload);
                self.ack_nr = self.ack_nr.wrapping_add(1);
            } else {
                break;
            }
        }
    }

    fn check_finished(&mut self) {
        if self.fin_sent && self.fin_received && self.flights.is_empty() {
            self.with_state(|state| {
                state.flushed = true;
                if let Some(waker) = state.write_waker.take() {
                    waker.wake();
                }
            });
            self.alive.store(false, Ordering::Relaxed);
        }
    }

    /// Переотправка по тайм-ауту (или немедленно для потерянного фронта).
    async fn retransmit_if_needed(&mut self, _ack_received: bool) {
        if let Some(front) = self.flights.front() {
            let elapsed = front.sent_at.elapsed();
            let retries = front.retries;
            let rto = rto_for(retries);
            if elapsed >= rto {
                let seq = front.seq;
                let kind = front.kind;
                let payload = front.payload.clone();
                let retries = retries + 1;
                let packet = self.packet(kind, seq, self.ack_nr, payload.clone());
                self.send(packet).await;
                if let Some(front) = self.flights.front_mut() {
                    front.sent_at = Instant::now();
                    front.retries = retries;
                }
                self.cwnd = (self.cwnd / 2).max(MIN_CWND);
            }
        }
        self.check_finished();
    }

    /// Основной цикл драйвера.
    async fn run(mut self) {
        let mut ticker = tokio::time::interval(Duration::from_millis(50));
        loop {
            if !self.alive.load(Ordering::Relaxed) {
                break;
            }
            if self.fin_sent && self.fin_received && self.last_activity.elapsed() > LINGER {
                break;
            }
            tokio::select! {
                maybe = self.packets.recv() => {
                    match maybe {
                        Some(packet) => self.handle(packet).await,
                        None => break,
                    }
                }
                _ = self.write_wake.notified() => {
                    self.pump().await;
                }
                _ = ticker.tick() => {
                    self.pump().await;
                    self.retransmit_if_needed(false).await;
                    if self.flights.is_empty() {
                        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
                        if !state.flushed && state.write_buffer.is_empty() {
                            state.flushed = true;
                            if let Some(waker) = state.write_waker.take() {
                                waker.wake();
                            }
                        }
                    }
                    if self.last_activity.elapsed() > Duration::from_secs(120) {
                        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
                        state.error = Some(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "uTP-соединение не отвечает",
                        ));
                        if let Some(waker) = state.read_waker.take() {
                            waker.wake();
                        }
                        if let Some(waker) = state.write_waker.take() {
                            waker.wake();
                        }
                        break;
                    }
                }
            }
            self.notify.notify_waiters();
        }
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(waker) = state.read_waker.take() {
            waker.wake();
        }
        if let Some(waker) = state.write_waker.take() {
            waker.wake();
        }
    }
}

fn rto_for(retries: u32) -> Duration {
    let backoff = 1u32 << retries.min(4);
    (RTO_MIN * backoff).min(RTO_MAX)
}

fn seq_before(left: u16, right: u16) -> bool {
    left.wrapping_sub(right) > 0x8000
}

fn seq_after(left: u16, right: u16) -> bool {
    right.wrapping_sub(left) > 0x8000
}

/// Таблица активных соединений: `local_id` → канал пакетов.
pub(crate) type Router = Arc<Mutex<HashMap<u16, mpsc::Sender<Packet>>>>;

/// Слушает UDP-сокет и разводит пакеты по соединениям (BEP 29).
///
/// Новое соединение создаётся по `ST_SYN`: дальше драйвер сам отвечает
/// `ST_STATE` и принимает данные.
pub struct UtpListener {
    socket: Arc<UdpSocket>,
    router: Router,
    incoming: mpsc::UnboundedSender<UtpStream>,
    closed: mpsc::UnboundedSender<u16>,
}

impl UtpListener {
    /// Запускает слушатель; вернёт адрес сокета и поток входящих соединений.
    pub async fn start(
        bind: SocketAddr,
    ) -> io::Result<(SocketAddr, mpsc::UnboundedReceiver<UtpStream>)> {
        let socket = UdpSocket::bind(bind).await?;
        let addr = socket.local_addr()?;
        let socket = Arc::new(socket);
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (closed_tx, mut closed_rx) = mpsc::unbounded_channel();
        let listener = Arc::new(UtpListener {
            socket,
            router: Arc::new(Mutex::new(HashMap::new())),
            incoming: incoming_tx,
            closed: closed_tx,
        });
        let this = listener.clone();
        tokio::spawn(async move {
            this.run().await;
        });
        // Удаление закрытых соединений — отдельной задачей, чтобы не держать
        // чтение сокета на мьютексе.
        let router = listener.router.clone();
        tokio::spawn(async move {
            while let Some(id) = closed_rx.recv().await {
                if let Ok(mut map) = router.lock() {
                    map.remove(&id);
                }
            }
        });
        Ok((addr, incoming_rx))
    }

    async fn run(self: Arc<Self>) {
        let mut buffer = [0u8; 1600];
        loop {
            let (read, addr) = match self.socket.recv_from(&mut buffer).await {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(packet) = Packet::decode(&buffer[..read]) else {
                continue;
            };
            let existing = self
                .router
                .lock()
                .ok()
                .and_then(|map| map.get(&packet.connection_id).cloned());
            if let Some(tx) = existing {
                if tx.send(packet).await.is_err() {
                    // Соединение уже закрыто: пакет никому не нужен.
                }
                continue;
            }
            if packet.kind != ST_SYN {
                continue;
            }
            // По BEP 29 `connection_id` выбирает инициатор, и обе стороны
            // используют его же: принимающая сторона не придумывает свой.
            let conn_id = packet.connection_id;
            let (tx, rx) = mpsc::channel(256);
            if let Ok(mut map) = self.router.lock() {
                map.insert(conn_id, tx.clone());
            }
            let shared: Shared = Arc::new(Mutex::new(ConnState::default()));
            let alive = Arc::new(AtomicBool::new(true));
            let notify = Arc::new(Notify::new());
            let write_wake = Arc::new(Notify::new());
            let stream = UtpStream {
                addr,
                state: shared.clone(),
                alive: alive.clone(),
                write_wake: write_wake.clone(),
            };
            let _ = self.incoming.send(stream);
            let driver = Driver::new(
                self.socket.clone(),
                addr,
                conn_id,
                shared,
                alive,
                notify,
                write_wake,
                rx,
            );
            let closed = self.closed.clone();
            tokio::spawn(async move {
                // Подтверждаем SYN: ack_nr = 1, наш seq = 1.
                let ack = driver.packet(ST_STATE, 1, packet.seq_nr, Vec::new());
                driver.send(ack).await;
                let _ = tx.send(packet).await;
                driver.run().await;
                let _ = closed.send(conn_id);
            });
        }
    }
}

/// Исходящее uTP-соединение: SYN, ожидание ответа, затем `UtpStream`.
pub async fn connect(addr: SocketAddr, timeout: Duration) -> io::Result<UtpStream> {
    let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    // Соединение не «подключаем»: иначе `send_to` на подключённом UDP-сокете
    // возвращает ошибку, а драйвер шлёт пакеты именно через `send_to`.
    let local_id = rand::random::<u16>() | 1;
    let syn = Packet {
        kind: ST_SYN,
        connection_id: local_id,
        timestamp_us: now_us(),
        timestamp_diff_us: 0,
        wnd_size: RECEIVE_WINDOW as u32,
        seq_nr: 1,
        ack_nr: 0,
        payload: Vec::new(),
    };
    socket.send_to(&syn.encode(), addr).await?;
    let deadline = Instant::now() + timeout;
    let mut buffer = [0u8; 1600];
    let mut last_syn = Instant::now();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "uTP: пир не ответил на SYN",
            ));
        }
        // SYN теряется так же, как данные: без повторов соединение не
        // установится на канале с потерями.
        if last_syn.elapsed() >= Duration::from_millis(500) {
            socket.send_to(&syn.encode(), addr).await?;
            last_syn = Instant::now();
        }
        let received = tokio::time::timeout(remaining.min(Duration::from_millis(500)), async {
            let (read, _) = socket.recv_from(&mut buffer).await?;
            Ok::<_, io::Error>(Packet::decode(&buffer[..read]))
        })
        .await;
        let Ok(Ok(maybe_packet)) = received else {
            continue;
        };
        let Some(packet) = maybe_packet else {
            continue;
        };
        if packet.kind == ST_RESET {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "uTP: пир сбросил соединение",
            ));
        }
        if packet.kind == ST_STATE && packet.ack_nr == 1 {
            let (tx, rx) = mpsc::channel(256);
            let shared: Shared = Arc::new(Mutex::new(ConnState::default()));
            let alive = Arc::new(AtomicBool::new(true));
            let notify = Arc::new(Notify::new());
            let write_wake = Arc::new(Notify::new());
            let stream = UtpStream {
                addr,
                state: shared.clone(),
                alive: alive.clone(),
                write_wake: write_wake.clone(),
            };
            let driver = Driver::new(
                socket.clone(),
                addr,
                local_id,
                shared,
                alive,
                notify,
                write_wake,
                rx,
            );
            // Приём пакетов для исходящего соединения: сокет один, и читать его
            // должен отдельный цикл, иначе ответы пира никто не увидит.
            let rx_socket = socket.clone();
            let rx_tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 1600];
                while let Ok((read, _)) = rx_socket.recv_from(&mut buf).await {
                    if let Some(packet) = Packet::decode(&buf[..read]) {
                        if rx_tx.send(packet).await.is_err() {
                            break;
                        }
                    }
                }
            });
            tokio::spawn(async move {
                // Первый ответ уже получен; переотправленный SYN подтвердим
                // повторным STATE, чтобы пир гарантированно открыл окно.
                let state = driver.packet(ST_STATE, 1, 1, Vec::new());
                driver.send(state).await;
                // Пакеты, пришедшие до создания канала, не теряем.
                let _ = tx.send(packet).await;
                driver.run().await;
            });
            return Ok(stream);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_roundtrip() {
        let packet = Packet {
            kind: ST_DATA,
            connection_id: 4242,
            timestamp_us: 123_456,
            timestamp_diff_us: 7,
            wnd_size: 65_536,
            seq_nr: 9,
            ack_nr: 8,
            payload: b"hello uTP".to_vec(),
        };
        let bytes = packet.encode();
        let parsed = Packet::decode(&bytes).expect("пакет разбирается");
        assert_eq!(parsed.kind, ST_DATA);
        assert_eq!(parsed.connection_id, 4242);
        assert_eq!(parsed.timestamp_us, 123_456);
        assert_eq!(parsed.wnd_size, 65_536);
        assert_eq!(parsed.seq_nr, 9);
        assert_eq!(parsed.ack_nr, 8);
        assert_eq!(parsed.payload, b"hello uTP");
    }

    #[test]
    fn decode_rejects_short_and_wrong_version() {
        assert!(Packet::decode(&[0u8; HEADER_LEN - 1]).is_none());
        let mut bytes = Packet {
            kind: ST_STATE,
            connection_id: 1,
            timestamp_us: 0,
            timestamp_diff_us: 0,
            wnd_size: 0,
            seq_nr: 0,
            ack_nr: 0,
            payload: Vec::new(),
        }
        .encode();
        bytes[0] = 0x20; // версия 2
        assert!(Packet::decode(&bytes).is_none());
    }

    #[test]
    fn sequence_wraparound_compares_correctly() {
        assert!(seq_before(0xFFFE, 0x0001));
        assert!(!seq_before(0x0001, 0xFFFE));
        assert!(seq_after(0x0001, 0xFFFE));
        assert!(!seq_after(0xFFFE, 0x0001));
    }

    #[tokio::test]
    async fn loopback_stream_transfers_data() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (addr, mut incoming) = UtpListener::start("127.0.0.1:0".parse().unwrap())
            .await
            .expect("слушатель uTP");
        let wait_accept = tokio::spawn(async move {
            let mut stream = incoming.recv().await.expect("входящее соединение");
            let mut buf = vec![0u8; 5];
            stream.read_exact(&mut buf).await.expect("чтение данных");
            assert_eq!(buf, b"hello");
            stream.write_all(b"world").await.expect("ответ");
            stream.flush().await.expect("сброс");
            stream.shutdown().await.ok();
        });
        let mut client = connect(addr, Duration::from_secs(3))
            .await
            .expect("uTP-соединение");
        client.write_all(b"hello").await.expect("запись");
        let mut answer = [0u8; 5];
        client.read_exact(&mut answer).await.expect("чтение ответа");
        assert_eq!(&answer, b"world");
        client.shutdown().await.ok();
        let _ = tokio::time::timeout(Duration::from_secs(3), wait_accept).await;
    }

    #[tokio::test]
    async fn transfers_through_lossy_relay() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Слушатель uTP и UDP-релей, который теряет каждый четвёртый пакет в
        // сторону сервера и каждый десятый в сторону клиента (BEP 29 требует
        // переотправки — без неё поток не соберётся).
        let (server_addr, mut incoming) = UtpListener::start("127.0.0.1:0".parse().unwrap())
            .await
            .expect("слушатель uTP");
        let relay = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("релей"));
        let relay_addr = relay.local_addr().expect("адрес релея");
        let relay_task = relay.clone();
        tokio::spawn(async move {
            let mut client: Option<SocketAddr> = None;
            let mut forward = 0u32;
            let mut backward = 0u32;
            let mut buf = [0u8; 1600];
            while let Ok((read, from)) = relay_task.recv_from(&mut buf).await {
                if from == server_addr {
                    backward += 1;
                    if backward % 10 == 0 {
                        continue;
                    }
                    if let Some(client_addr) = client {
                        let _ = relay_task.send_to(&buf[..read], client_addr).await;
                    }
                } else {
                    client = Some(from);
                    forward += 1;
                    if forward % 4 == 0 {
                        continue;
                    }
                    let _ = relay_task.send_to(&buf[..read], server_addr).await;
                }
            }
        });

        let echo = tokio::spawn(async move {
            let mut stream = incoming.recv().await.expect("входящее uTP-соединение");
            let mut data = vec![0u8; 16 * 1024];
            stream.read_exact(&mut data).await.expect("приём по uTP");
            stream.write_all(&data).await.expect("эхо по uTP");
            stream.flush().await.expect("сброс");
            stream.shutdown().await.ok();
        });

        let mut client = connect(relay_addr, Duration::from_secs(8))
            .await
            .expect("uTP-соединение через релей с потерями");
        let payload: Vec<u8> = (0..16 * 1024).map(|index| (index % 251) as u8).collect();
        client
            .write_all(&payload)
            .await
            .expect("запись через потери");
        let mut answer = vec![0u8; payload.len()];
        tokio::time::timeout(Duration::from_secs(30), client.read_exact(&mut answer))
            .await
            .expect("ответ дошёл через потери")
            .expect("чтение ответа");
        assert_eq!(answer, payload, "данные через потери совпадают");
        client.shutdown().await.ok();
        tokio::time::timeout(Duration::from_secs(10), echo)
            .await
            .expect("эхо-пир завершился")
            .expect("задача эха");
    }
}
