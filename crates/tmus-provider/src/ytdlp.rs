//! Общий хелпер вызова `yt-dlp`.
//!
//! Общий — потому что им пользуются и YouTube Music, и SoundCloud; Spotify
//! воспользоваться им не может (там свой протокол, нужен librespot), поэтому
//! хелпер — обычная структура, а не трейт: провайдер берёт её по желанию.
//!
//! Замеры, на которых держатся решения (17.09.2026):
//! - резолв трека `yt-dlp -J --extractor-args 'youtube:player_client=web_music'
//!   -f 'bestaudio[acodec=opus]/bestaudio'` → itag 774, opus, abr 251.12;
//! - `--flat-playlist` на плейлисте лайков YTM → 1.36 с.
//!
//! Грабли, учтённые здесь:
//! - `player_client` обязан быть запинен вызывающим: дефолт yt-dlp уходит
//!   в `web_creator`, URL резолвится, а GET по нему даёт 403. С волны
//!   403 от googlevideo (20.09.2026, yt-dlp #17682/#17705) пинится уже
//!   цепочка клиентов — см. `YtMusic::resolve`; здесь хелпер про то,
//!   как честно дождаться/убить процесс;
//! - из заголовков ответа берётся ТОЛЬКО `User-Agent` — см. комментарий
//!   в [`parse_media`];
//! - ссылка потока живёт ~6 ч (`expire=`), поэтому кэшировать URL нельзя
//!   никогда — только метаданные и файлы.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::ProviderError;
use tmus_core::model::{ProviderId, StreamSource};

/// Срок жизни зависшего вызова yt-dlp. Замедленный резолв терпим, а вот
/// зависший — вешает плеер, поэтому обрывается.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Подготовленный вызов yt-dlp: путь к бинарю + общий таймаут.
pub struct YtDlp {
    binary: PathBuf,
    timeout: Duration,
}

/// Что резолвим и как.
pub struct YtDlpRequest<'a> {
    pub provider: ProviderId,
    pub page_url: &'a str,
    /// Например `"bestaudio[acodec=opus]/bestaudio"`.
    pub format: &'a str,
    /// Пары для `--extractor-args`, например `["youtube:player_client=web_music"]`.
    pub extractor_args: &'a [&'a str],
    pub cookies: Option<&'a tmus_core::cookies::CookieSource>,
}

/// Выбранный поток из `-J`-ответа.
pub struct YtDlpMedia {
    pub url: String,
    pub user_agent: Option<String>,
    pub expires_at: Option<std::time::SystemTime>,
    pub ext: String,
    pub abr: Option<f64>,
    pub acodec: Option<String>,
}

impl YtDlp {
    #[must_use]
    pub fn new(binary: PathBuf) -> Self {
        Self {
            binary,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Переопределить таймаут (по умолчанию 60 с).
    #[must_use]
    pub fn timeout(mut self, dur: Duration) -> Self {
        self.timeout = dur;
        self
    }

    /// Полный разбор страницы: `yt-dlp -J`. Ожидается, что yt-dlp выберет
    /// формат по `req.format` — наружу уходит только выбранный элемент.
    pub async fn media(&self, req: &YtDlpRequest<'_>) -> crate::Result<YtDlpMedia> {
        let mut args: Vec<String> = vec!["--no-warnings".into(), "--quiet".into(), "-J".into()];
        args.push("-f".into());
        args.push(req.format.into());
        for ea in req.extractor_args {
            args.push("--extractor-args".into());
            args.push((*ea).into());
        }
        if let Some(cookies) = req.cookies {
            args.extend(cookies.as_ytdlp_args());
        }
        args.push(req.page_url.into());

        let out = self.run(&args, req.provider).await?;
        let value: serde_json::Value = serde_json::from_str(&out)
            .map_err(|e| ProviderError::Format {
                provider: req.provider,
                reason: format!("yt-dlp вернул не-JSON: {e}"),
            })?;
        parse_media(req.provider, &value)
    }

    /// Запуск с таймаутом и разбором статуса. `provider` нужен только
    /// для сообщений об ошибках.
    async fn run(&self, args: &[String], provider: ProviderId) -> crate::Result<String> {
        let mut cmd = Command::new(&self.binary);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Отмена обязана убивать процесс, а не отпускать его на волю.
            // Без этого брошенный резолв (отменённая предзагрузка при
            // скипе, истёкший таймаут) оставлял бы yt-dlp доживать своё:
            // 0.9 CPU-с и 335 МБ на процесс, замерено 18.09.2026.
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| ProviderError::Tool {
                tool: "yt-dlp",
                reason: format!("не удалось запустить {}: {e}", self.binary.display()),
            })?;

        // stdout/stderr забираются из child ДО ожидания и читаются
        // конкурентно: `wait_with_output` под timeout при срабатывании
        // дропал child вместе с незачитанными пайпами — процесс убивался
        // kill_on_drop, но не реапился (в журнале: зомби yt-dlp, Stat Z,
        // 3 ч 23 мин, RSS 0, 20.09.2026). Конкурентное чтение здесь ещё и
        // от дедлока: `-J`-ответ — мегабайты, пайп (64 КиБ) забивается,
        // yt-dlp висит на записи, а мы на wait() — классический тупик,
        // если читать stdout только после завершения.
        let mut stdout_pipe = child
            .stdout
            .take()
            .expect("stdout запрошен пайпом при спавне");
        let mut stderr_pipe = child
            .stderr
            .take()
            .expect("stderr запрошен пайпом при спавне");
        let stdout_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut buf).await;
            buf
        });
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf).await;
            buf
        });

        let status = match timeout(self.timeout, child.wait()).await {
            Ok(status) => status.map_err(|e| ProviderError::Tool {
                tool: "yt-dlp",
                reason: format!("сбой ввода-вывода: {e}"),
            })?,
            Err(_) => {
                // kill_on_drop дропнутого child не реапит: убитый процесс
                // остаётся зомби, пока жив родитель. Поэтому явные
                // kill + wait — пара гарантирует, что потомка нет и в
                // таблице процессов.
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(ProviderError::Tool {
                    tool: "yt-dlp",
                    reason: format!("yt-dlp не ответил за {}с", self.timeout.as_secs()),
                });
            }
        };

        let stdout = stdout_task
            .await
            .expect("задача чтения stdout не может запаниковать");
        let stderr = stderr_task
            .await
            .expect("задача чтения stderr не может запаниковать");

        if !status.success() {
            let stderr = String::from_utf8_lossy(&stderr);
            return Err(stderr_error(provider, &stderr));
        }
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }

    /// Чистая функция: `YtDlpMedia` → `StreamSource`. Без io, чтобы
    /// провайдер мог строить источник и из кэшированного медиа-описания.
    #[must_use]
    pub fn to_stream_source(media: YtDlpMedia) -> StreamSource {
        StreamSource::Remote {
            url: media.url,
            user_agent: media.user_agent,
            expires_at: media.expires_at,
        }
    }
}

/// Ошибка из stderr yt-dlp. Весь stderr в сообщение не тащим: он
/// многострочный и не влезает. Берём последнюю непустую строку — yt-dlp
/// пишет туда человекочитаемую причину.
///
/// Бот-гейт распознаётся отдельно: «Sign in to confirm you're not a bot»
/// — это не поломка инструмента, а отсутствующая/истёкшая сессия, чинится
/// переподключением аккаунта. Замерено, что yt-dlp отдаёт это сообщение на
/// всех клиентах при отсутствии cookies, поэтому его нельзя отдавать как
/// `Tool` — вызывающий должен предложить переподключение, а не «переустановить yt-dlp».
fn stderr_error(provider: ProviderId, stderr: &str) -> ProviderError {
    let last = stderr.lines().rev().find(|l| !l.trim().is_empty());
    let reason = last.unwrap_or("нет объяснения в stderr").trim().to_owned();
    let bot_gate = stderr.contains("Sign in to confirm")
        || stderr.contains("confirm you're not a bot");
    if bot_gate {
        ProviderError::Auth { provider, reason }
    } else {
        ProviderError::Tool {
            tool: "yt-dlp",
            reason,
        }
    }
}

/// Выбранный формат — первый элемент `requested_downloads` (yt-dlp кладёт
/// туда то, что реально скачал бы); его нет — значит формат не выбрался.
fn parse_media(provider: ProviderId, value: &serde_json::Value) -> crate::Result<YtDlpMedia> {
    let fmt = value
        .get("requested_downloads")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .ok_or(ProviderError::Format {
            provider,
            reason: "в ответе нет requested_downloads".into(),
        })?;

    let url = fmt
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or(ProviderError::Format {
            provider,
            reason: "в выбранном формате нет url".into(),
        })?
        .to_owned();

    Ok(YtDlpMedia {
        user_agent: fmt
            .get("http_headers")
            .and_then(|h| h.get("User-Agent"))
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        expires_at: parse_expire(&url),
        ext: fmt
            .get("ext")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned(),
        abr: fmt.get("abr").and_then(serde_json::Value::as_f64),
        acodec: fmt
            .get("acodec")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        url,
    })
}

/// `expires_at` из параметра `expire=<unix_ts>` в URL. Нет параметра —
/// `None`: незнакомые провайдеры не обязаны иметь срок, а googlevideo-ссылка
/// (~6 ч) без учёта срока неизбежно протухнет в кэше.
fn parse_expire(url: &str) -> Option<std::time::SystemTime> {
    let start = url.split(&['?', '&']).find_map(|q| q.strip_prefix("expire="))?;
    let ts: u64 = start.parse().ok()?;
    Some(std::time::UNIX_EPOCH + Duration::from_secs(ts))
}

#[cfg(test)]
mod tests {
    use super::*;

    const YTM: ProviderId = ProviderId::YTMUSIC;

    /// Зашитый фрагмент ответа `yt-dlp -J`. `Accept` с запятыми — это
    /// регресс на замеренную граблю: mpv разрезает заголовки по запятым.
    const MEDIA_JSON: &str = r#"{
        "requested_downloads": [{
            "url": "https://rr3---sn.googlevideo.com/videoplayback?id=abc&expire=1893456000&itag=774",
            "ext": "webm",
            "abr": 251.12,
            "acodec": "opus",
            "http_headers": {
                "User-Agent": "com.google.android.youtube/19.09.37",
                "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
            }
        }]
    }"#;

    #[test]
    fn media_parses_selected_format() {
        let v: serde_json::Value = serde_json::from_str(MEDIA_JSON).unwrap();
        let m = parse_media(YTM, &v).unwrap();
        assert!(m.url.contains("videoplayback"));
        assert_eq!(m.ext, "webm");
        assert_eq!(m.abr, Some(251.12));
        assert_eq!(m.acodec.as_deref(), Some("opus"));
    }

    #[test]
    fn only_user_agent_is_taken() {
        let v: serde_json::Value = serde_json::from_str(MEDIA_JSON).unwrap();
        let m = parse_media(YTM, &v).unwrap();
        // Accept с запятыми не должен протечь: в mpv он распадается и
        // googlevideo отвечает 400 Bad Request.
        assert_eq!(m.user_agent.as_deref(), Some("com.google.android.youtube/19.09.37"));
    }

    #[test]
    fn expire_param_becomes_expires_at() {
        let url = "https://x/videoplayback?expire=1893456000&itag=774";
        let at = parse_expire(url).unwrap();
        assert_eq!(
            at.duration_since(std::time::UNIX_EPOCH).unwrap(),
            Duration::from_secs(1_893_456_000)
        );
    }

    #[test]
    fn url_without_expire_is_not_expiring() {
        assert_eq!(parse_expire("https://x/videoplayback?itag=774"), None);
    }

    #[test]
    fn to_stream_source_yields_remote_and_expiry() {
        let media = YtDlpMedia {
            url: "https://x/videoplayback?expire=1893456000".into(),
            user_agent: Some("ua".into()),
            expires_at: parse_expire("https://x/?expire=1893456000"),
            ext: "webm".into(),
            abr: None,
            acodec: None,
        };
        let src = YtDlp::to_stream_source(media);
        let StreamSource::Remote { url, user_agent, expires_at } = &src else {
            panic!("ожидался Remote");
        };
        assert_eq!(url, "https://x/videoplayback?expire=1893456000");
        assert_eq!(user_agent.as_deref(), Some("ua"));
        // Ссылка с expire в будущем жива; с истёкшим сроком — мертва.
        let now = std::time::SystemTime::now();
        assert!(!src.is_expired(now));
        let past = std::time::UNIX_EPOCH;
        let expired = StreamSource::Remote {
            url: "https://x/".into(),
            user_agent: None,
            expires_at: Some(past),
        };
        assert!(expired.is_expired(now));
        let _ = expires_at;
    }

    #[test]
    fn extractor_and_cookie_args_are_passed() {
        let cookies = tmus_core::cookies::CookieSource::Browser {
            spec: "chromium:/home/user/.config/ytm/profile".into(),
        };
        // Клиент в тесте — первый из цепочки резолва (web_embedded):
        // единственный переживший волну 403 от googlevideo 20.09.2026.
        let req = YtDlpRequest {
            provider: YTM,
            page_url: "https://www.youtube.com/watch?v=abc",
            format: "bestaudio[acodec=opus]/bestaudio",
            extractor_args: &["youtube:player_client=web_embedded"],
            cookies: Some(&cookies),
        };

        let mut args: Vec<String> = vec!["-f".into(), req.format.into()];
        for ea in req.extractor_args {
            args.push("--extractor-args".into());
            args.push((*ea).into());
        }
        args.extend(cookies.as_ytdlp_args());

        // extractor_args идут парами флаг+значение; cookies-аргументы присутствуют.
        assert!(args.windows(2).any(|w| w[0] == "--extractor-args"
            && w[1] == "youtube:player_client=web_embedded"));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--cookies-from-browser"
                && w[1].starts_with("chromium:")));
    }

    /// Таймаут обязан не только вернуть ошибку, но и не оставить
    /// процесса: раньше брошенный `wait_with_output` убивал потомка
    /// kill_on_drop без реапа — зомби yt-dlp жил в таблице процессов
    /// часами (Stat Z, 3 ч 23 мин, 20.09.2026).
    #[tokio::test]
    async fn timeout_kills_and_reaps_child() {
        let dlp = YtDlp::new(PathBuf::from("/bin/sleep")).timeout(Duration::from_millis(100));
        let started = std::time::Instant::now();
        let err = dlp.run(&["30".into()], YTM).await.unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "таймаут должен сработать быстро, а не через 30 с"
        );
        match err {
            ProviderError::Tool { tool, reason } => {
                assert_eq!(tool, "yt-dlp");
                assert!(reason.contains("не ответил"), "неожиданная причина: {reason}");
            }
            other => panic!("ожидался Tool, получен {other:?}"),
        }
        // sleep всё ещё в таблице процессов = kill/wait-пара не сработала.
        // Даём ядру секунду добрать реап — SIGKILL мгновенный, но
        // планировщик не мгновенный.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let zombies: Vec<String> = std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
            .filter(|stat| {
                // поле 3 — состояние; имя в скобках может содержать
                // пробелы, поэтому ищем с конца: ") Z"
                stat.contains(") Z") && stat.contains("(sleep)")
            })
            .collect();
        assert!(zombies.is_empty(), "остались зомби sleep: {zombies:?}");
    }

    #[test]
    fn bot_gate_maps_to_auth() {
        let err = stderr_error(
            YTM,
            "ERROR: [youtube] abc: Sign in to confirm you're not a bot.\n",
        );
        assert!(matches!(err, ProviderError::Auth { provider: YTM, .. }));
    }

    #[test]
    fn plain_failure_maps_to_tool_with_last_stderr_line() {
        let err = stderr_error(
            YTM,
            "WARNING: something\nERROR: unable to download video data: HTTP Error 403\n",
        );
        match err {
            ProviderError::Tool { tool, reason } => {
                assert_eq!(tool, "yt-dlp");
                assert_eq!(reason, "ERROR: unable to download video data: HTTP Error 403");
            }
            other => panic!("ожидался Tool, получен {other:?}"),
        }
    }
}
