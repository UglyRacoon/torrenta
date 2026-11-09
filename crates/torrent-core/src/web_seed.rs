//! Web seeds (BEP 19): загрузка кусков торрента по HTTP(S) с обычных веб-серверов.
//!
//! Модуль намеренно ничего не знает про метаинформацию: он умеет только забирать
//! диапазон байт из файла, доступного по заданному URL, и разбирать поле
//! `url-list` из `.torrent` (формат списка адресов — часть BEP 19).
//!
//! Схема работы по BEP 19: клиент запрашивает у сервера диапазон
//! `Range: bytes=<offset>-<offset+len-1>` и получает либо `206 Partial Content`
//! с запрошенным куском, либо `200 OK` с файлом целиком (сервер проигнорировал
//! `Range`) — во втором случае нужный диапазон вырезается из тела на лету.

use std::time::Duration;

use thiserror::Error;

use crate::bencode::Value;

/// Настройки одного web seed.
#[derive(Debug, Clone)]
pub struct WebSeedConfig {
    /// Таймаут установки TCP/TLS-соединения, сек.
    pub connect_timeout: Duration,
    /// Предельное ожидание заголовков ответа и каждого следующего блока тела.
    pub read_timeout: Duration,
    /// Сколько диапазонов разрешено качать параллельно (использует планировщик движка).
    pub max_parallel: usize,
}

impl Default for WebSeedConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(30),
            max_parallel: 4,
        }
    }
}

/// Ошибки web seed.
#[derive(Debug, Error)]
pub enum WebSeedError {
    /// Строка не является http(s)-адресом.
    #[error("некорректный URL web seed: {0}")]
    BadUrl(String),
    /// Сетевая ошибка, ошибка прокси или построения HTTP-клиента.
    #[error("ошибка HTTP-запроса к web seed: {0}")]
    Http(String),
    /// Сервер ответил статусом, отличным от 200 и 206.
    #[error("web seed вернул статус {0}")]
    Status(u16),
    /// Сервер отдал меньше байт, чем нужно для запрошенного диапазона.
    #[error("web seed отдал меньше данных, чем нужно для диапазона")]
    ShortBody,
    /// Сервер вернул `206` не с того смещения, которое запросили.
    #[error("web seed вернул диапазон с {got}, а запрошен с {want}")]
    RangeMismatch { want: u64, got: u64 },
    /// Диапазон выходит за границы `u64` (offset + len переполняет адресацию).
    #[error("некорректный диапазон: смещение {offset}, длина {len}")]
    BadRange { offset: u64, len: u32 },
    /// Сервер не ответил за отведённое время.
    #[error("web seed не ответил за отведённое время")]
    Timeout,
}

/// Один web seed: базовый URL плюс готовый HTTP-клиент с таймаутами и прокси.
#[derive(Debug, Clone)]
pub struct WebSeed {
    url: String,
    client: reqwest::Client,
    config: WebSeedConfig,
}

impl WebSeed {
    /// Создаёт web seed для базового URL из `url-list`.
    ///
    /// `url` может заканчиваться на `/` — как его отдал трекер/метаинформация;
    /// склейку базового адреса с именем файла торрента делает вызывающая сторона.
    /// `proxy` — необязательная строка в формате reqwest (`http://user:pass@host:port`
    /// или `socks5://host:port`).
    pub fn new(
        url: &str,
        proxy: Option<&str>,
        config: WebSeedConfig,
    ) -> Result<Self, WebSeedError> {
        let url = url.trim();
        if !is_http_url(url) {
            return Err(WebSeedError::BadUrl(url.to_string()));
        }

        // Системные прокси из окружения отключаем всегда: у движка есть свой
        // явный источник настройки прокси, иначе поведение зависело бы от
        // переменных среды машины пользователя.
        let mut builder = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .redirect(reqwest::redirect::Policy::limited(5))
            .no_proxy();

        if let Some(spec) = proxy.map(str::trim).filter(|spec| !spec.is_empty()) {
            let proxy = reqwest::Proxy::all(spec)
                .map_err(|err| WebSeedError::Http(format!("некорректный прокси {spec}: {err}")))?;
            builder = builder.proxy(proxy);
        }

        let client = builder
            .build()
            .map_err(|err| WebSeedError::Http(format!("не удалось создать HTTP-клиент: {err}")))?;

        Ok(Self {
            url: url.to_string(),
            client,
            config,
        })
    }

    /// Базовый URL web seed ровно в том виде, в каком он был принят.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Забирает `len` байт начиная со смещения `offset` в файле web seed.
    ///
    /// `206` — сервер отдал ровно запрошенный диапазон (проверяем `Content-Range`);
    /// `200` — сервер проигнорировал `Range` и отдал файл целиком, тогда лишние
    /// байты до `offset` и после диапазона отбрасываются потоком.
    /// Любой другой статус — [`WebSeedError::Status`].
    pub async fn fetch_range(&self, offset: u64, len: u32) -> Result<Vec<u8>, WebSeedError> {
        if len == 0 {
            // Пустой запрос не имеет смысла, но и ошибкой не является.
            return Ok(Vec::new());
        }

        let want = len as usize;
        let last = offset
            .checked_add(u64::from(len) - 1)
            .ok_or(WebSeedError::BadRange { offset, len })?;

        let request = self
            .client
            .get(&self.url)
            .header(reqwest::header::RANGE, format!("bytes={offset}-{last}"));

        let mut response =
            match tokio::time::timeout(self.config.read_timeout, request.send()).await {
                Err(_) => return Err(WebSeedError::Timeout),
                Ok(Err(err)) => return Err(WebSeedError::Http(err.to_string())),
                Ok(Ok(response)) => response,
            };

        let status = response.status();
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            let available = match content_range_bounds(&response) {
                Some((start, end)) => {
                    if start != offset {
                        return Err(WebSeedError::RangeMismatch {
                            want: offset,
                            got: start,
                        });
                    }
                    end.saturating_sub(start) + 1
                }
                // Некоторые серверы не присылают Content-Range в 206: доверяем
                // запросу, но тело всё равно проверим на длину.
                None => u64::from(len),
            };
            if available < u64::from(len) {
                return Err(WebSeedError::ShortBody);
            }
            return self.read_range(&mut response, 0, want).await;
        }

        if status == reqwest::StatusCode::OK {
            return self.read_range(&mut response, offset, want).await;
        }

        Err(WebSeedError::Status(status.as_u16()))
    }

    /// Читает тело `response`, пропуская `skip` байт, и возвращает ровно `want` байт.
    ///
    /// Тело читается блоками, поэтому ответ `200` с файлом в гигабайт не тянет
    /// в память целиком: как только диапазон собран, чтение прекращается.
    async fn read_range(
        &self,
        response: &mut reqwest::Response,
        mut skip: u64,
        want: usize,
    ) -> Result<Vec<u8>, WebSeedError> {
        // Не выделяем память под весь диапазон: он может быть сколь угодно большим.
        let mut out = Vec::with_capacity(want.min(64 * 1024));
        while out.len() < want {
            let chunk = match tokio::time::timeout(self.config.read_timeout, response.chunk()).await
            {
                Err(_) => return Err(WebSeedError::Timeout),
                Ok(Err(err)) => return Err(WebSeedError::Http(err.to_string())),
                Ok(Ok(None)) => break,
                Ok(Ok(Some(chunk))) => chunk,
            };

            let mut data: &[u8] = &chunk;
            if skip > 0 {
                let step = skip.min(data.len() as u64) as usize;
                data = &data[step..];
                skip -= step as u64;
            }
            if data.is_empty() {
                continue;
            }

            let take = (want - out.len()).min(data.len());
            out.extend_from_slice(&data[..take]);
        }

        if out.len() < want {
            return Err(WebSeedError::ShortBody);
        }
        Ok(out)
    }
}

/// Допустим ли URL как web seed: только http/https и обязательный хост.
fn is_http_url(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(parsed) => matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some(),
        Err(_) => false,
    }
}

/// Границы диапазона из заголовка `Content-Range: bytes start-end/total`.
fn content_range_bounds(response: &reqwest::Response) -> Option<(u64, u64)> {
    let value = response.headers().get(reqwest::header::CONTENT_RANGE)?;
    parse_content_range(value.to_str().ok()?)
}

/// Разбирает `bytes 0-1023/4096` в пару `(0, 1023)`.
fn parse_content_range(header: &str) -> Option<(u64, u64)> {
    let rest = header.trim().strip_prefix("bytes")?.trim_start();
    let (range, _total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.trim().parse::<u64>().ok()?;
    let end = end.trim().parse::<u64>().ok()?;
    if end < start {
        return None;
    }
    Some((start, end))
}

/// Разбор поля `url-list` из метаинформации (BEP 19).
///
/// Значение — либо строка, либо список строк; берутся только http(s)-адреса,
/// пробелы по краям обрезаются, дубликаты сохраняются в порядке появления один раз.
pub fn parse_url_list(value: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let candidates: Vec<&Value> = match value {
        Value::List(items) => items.iter().collect(),
        single => vec![single],
    };

    for candidate in candidates {
        let Some(raw) = candidate.as_str() else {
            continue;
        };
        let url = raw.trim();
        if url.is_empty() || !is_http_url(url) {
            continue;
        }
        if out.iter().any(|known| known == url) {
            continue;
        }
        out.push(url.to_string());
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Одноразовый HTTP-сервер: читает запрос, отдаёт заданный ответ, закрывает соединение.
    ///
    /// Возвращает URL файла и задачу, которая отдаёт полученный запрос целиком.
    async fn one_shot_server(
        head: &'static str,
        body: Vec<u8>,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("не удалось занять локальный порт");
        let addr = listener
            .local_addr()
            .expect("у слушателя нет локального адреса");
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("сервер не получил соединение");
            let mut buffer = vec![0u8; 8192];
            let mut request = Vec::new();
            loop {
                let read = stream.read(&mut buffer).await.expect("сбой чтения запроса");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            stream
                .write_all(head.as_bytes())
                .await
                .expect("сбой записи заголовков");
            stream.write_all(&body).await.expect("сбой записи тела");
            stream.flush().await.expect("сбой сброса ответа");
            String::from_utf8_lossy(&request).into_owned()
        });
        (
            format!("http://127.0.0.1:{}/files/data.bin", addr.port()),
            handle,
        )
    }

    /// Web seed с короткими таймаутами, чтобы тесты не ждали секундами.
    fn seed(url: &str) -> WebSeed {
        seed_with_config(url, WebSeedConfig::default())
    }

    fn seed_with_config(url: &str, config: WebSeedConfig) -> WebSeed {
        WebSeed::new(url, None, config).expect("web seed должен создаваться")
    }

    #[tokio::test]
    async fn fetch_range_takes_slice_from_partial_content() {
        let (url, server) = one_shot_server(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4-7/16\r\nContent-Length: 4\r\nConnection: close\r\n\r\n",
            b"4567".to_vec(),
        )
        .await;

        let bytes = seed(&url)
            .fetch_range(4, 4)
            .await
            .expect("диапазон должен скачаться");
        let request = server.await.expect("сервер не завершился");

        assert_eq!(bytes, b"4567", "206 должен вернуть ровно запрошенный кусок");
        let request = request.to_ascii_lowercase();
        assert!(
            request.contains("range: bytes=4-7"),
            "в запросе должен быть Range: {request}"
        );
    }

    #[tokio::test]
    async fn fetch_range_slices_full_body_when_range_is_ignored() {
        let (url, server) = one_shot_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\n",
            b"0123456789abcdef".to_vec(),
        )
        .await;

        let bytes = seed(&url)
            .fetch_range(4, 4)
            .await
            .expect("на 200 диапазон вырезается из тела");
        server.await.expect("сервер не завершился");

        assert_eq!(bytes, b"4567", "из полного тела берётся нужный срез");
    }

    #[tokio::test]
    async fn fetch_range_reports_short_body_for_full_response() {
        let (url, server) = one_shot_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n",
            b"0123".to_vec(),
        )
        .await;

        let err = seed(&url)
            .fetch_range(4, 4)
            .await
            .expect_err("тело короче диапазона — это ошибка");
        server.await.expect("сервер не завершился");

        assert!(matches!(err, WebSeedError::ShortBody), "{err:?}");
    }

    #[tokio::test]
    async fn fetch_range_reports_short_body_for_partial_content() {
        let (url, server) = one_shot_server(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 4-7/16\r\nContent-Length: 2\r\nConnection: close\r\n\r\n",
            b"45".to_vec(),
        )
        .await;

        let err = seed(&url)
            .fetch_range(4, 4)
            .await
            .expect_err("206 с коротким телом — это ошибка");
        server.await.expect("сервер не завершился");

        assert!(matches!(err, WebSeedError::ShortBody), "{err:?}");
    }

    #[tokio::test]
    async fn fetch_range_surfaces_http_status() {
        let (url, server) = one_shot_server(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Vec::new(),
        )
        .await;

        let err = seed(&url)
            .fetch_range(0, 16)
            .await
            .expect_err("404 — это ошибка");
        server.await.expect("сервер не завершился");

        assert!(matches!(err, WebSeedError::Status(404)), "{err:?}");
    }

    #[tokio::test]
    async fn fetch_range_rejects_foreign_content_range() {
        let (url, server) = one_shot_server(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/16\r\nContent-Length: 4\r\nConnection: close\r\n\r\n",
            b"0123".to_vec(),
        )
        .await;

        let err = seed(&url)
            .fetch_range(4, 4)
            .await
            .expect_err("не тот диапазон — это ошибка");
        server.await.expect("сервер не завершился");

        assert!(
            matches!(err, WebSeedError::RangeMismatch { want: 4, got: 0 }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn fetch_range_returns_empty_for_zero_length() {
        // Сервер не нужен: запрос нулевой длины не уходит в сеть.
        let web_seed = seed("http://127.0.0.1:1/files/data.bin");
        let bytes = web_seed
            .fetch_range(100, 0)
            .await
            .expect("нулевой диапазон — не ошибка");
        assert!(bytes.is_empty(), "нулевой диапазон даёт пустой вектор");
    }

    #[tokio::test]
    async fn fetch_range_times_out_without_response() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("не удалось занять локальный порт");
        let addr = listener
            .local_addr()
            .expect("у слушателя нет локального адреса");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("сервер не получил соединение");
            let mut buffer = vec![0u8; 8192];
            let _ = stream.read(&mut buffer).await;
            // Держим соединение открытым дольше таймаута чтения.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let config = WebSeedConfig {
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_millis(200),
            ..WebSeedConfig::default()
        };
        let url = format!("http://127.0.0.1:{}/files/data.bin", addr.port());
        let err = seed_with_config(&url, config)
            .fetch_range(0, 16)
            .await
            .expect_err("молчащий сервер должен приводить к таймауту");
        server.abort();

        assert!(matches!(err, WebSeedError::Timeout), "{err:?}");
    }

    #[test]
    fn new_rejects_non_http_urls() {
        for url in [
            "",
            "   ",
            "не url",
            "ftp://example.com/f.bin",
            "file:///etc/passwd",
            "magnet:?xt=urn:btih:0000000000000000000000000000000000000000",
        ] {
            let err = WebSeed::new(url, None, WebSeedConfig::default())
                .expect_err("некорректный URL должен отвергаться");
            assert!(matches!(err, WebSeedError::BadUrl(_)), "{url}: {err:?}");
        }
    }

    #[test]
    fn new_accepts_http_urls_and_trims_whitespace() {
        for url in [
            "http://example.com/files/data.bin",
            "https://example.com/files/",
            "http://127.0.0.1:8080/data.bin",
        ] {
            let web_seed = WebSeed::new(&format!("  {url}  "), None, WebSeedConfig::default())
                .expect("корректный URL должен приниматься");
            assert_eq!(web_seed.url(), url, "URL сохраняется без пробелов по краям");
        }
    }

    #[test]
    fn new_rejects_garbage_proxy() {
        let err = WebSeed::new(
            "http://example.com/data.bin",
            Some("не прокси"),
            WebSeedConfig::default(),
        )
        .expect_err("мусорная строка прокси — это ошибка");
        assert!(matches!(err, WebSeedError::Http(_)), "{err:?}");

        // Пустая строка и пробелы означают «прокси нет» — это не ошибка.
        let web_seed = WebSeed::new(
            "http://example.com/data.bin",
            Some("   "),
            WebSeedConfig::default(),
        )
        .expect("пустой прокси допустим");
        assert_eq!(web_seed.url(), "http://example.com/data.bin");
    }

    #[test]
    fn parse_url_list_accepts_single_string() {
        let value = Value::string("  http://example.com/files/data.bin  ");
        assert_eq!(
            parse_url_list(&value),
            vec!["http://example.com/files/data.bin".to_string()]
        );

        let https = Value::string("https://example.com/dir/");
        assert_eq!(
            parse_url_list(&https),
            vec!["https://example.com/dir/".to_string()]
        );
    }

    #[test]
    fn parse_url_list_filters_list_and_dedupes() {
        let value = Value::List(vec![
            Value::string(" http://a.example/1.bin "),
            Value::string("https://b.example/2.bin"),
            Value::string("ftp://c.example/3.bin"),
            Value::string("не url"),
            Value::string("http://a.example/1.bin"),
            Value::int(7),
        ]);

        assert_eq!(
            parse_url_list(&value),
            vec![
                "http://a.example/1.bin".to_string(),
                "https://b.example/2.bin".to_string(),
            ],
            "только http(s), без повторов, в порядке появления"
        );
    }

    #[test]
    fn parse_url_list_ignores_unexpected_values() {
        assert!(parse_url_list(&Value::int(42)).is_empty());
        assert!(parse_url_list(&Value::Dict(BTreeMap::new())).is_empty());
        assert!(parse_url_list(&Value::List(vec![Value::int(1), Value::int(2)])).is_empty());
        assert!(parse_url_list(&Value::string("   ")).is_empty());
    }

    #[test]
    fn parse_content_range_parses_valid_headers() {
        assert_eq!(parse_content_range("bytes 0-1023/4096"), Some((0, 1023)));
        assert_eq!(parse_content_range("bytes 10-10/*"), Some((10, 10)));
        assert_eq!(parse_content_range(" bytes  4-7/16 "), Some((4, 7)));
        assert_eq!(parse_content_range("items 0-3/16"), None);
        assert_eq!(parse_content_range("bytes 9-4/16"), None);
        assert_eq!(parse_content_range("bytes 0-3"), None);
        assert_eq!(parse_content_range(""), None);
    }
}
