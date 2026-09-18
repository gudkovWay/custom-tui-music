//! mpv как дочерний процесс и JSON-IPC с ним.
//!
//! mpv запускается подпроцессом, а не через libmpv, по двум замеренным
//! причинам: краш плеера не убивает демон, и mpv уже решил seek, gapless
//! и вывод в PipeWire — своё аудио на rodio/symphonia это тот путь, на
//! котором конкурирующий проект youtui застрял на gapless.
//!
//! IPC — newline-delimited JSON по unix-сокету: запрос
//! `{"command":[...],"request_id":N}`, ответ повторяет `request_id`,
//! события приходят без него — по этому признаку они и различаются.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tmus_core::model::StreamSource;
use tmus_core::paths::Paths;

/// Сокет должен появиться после spawn; connect сразу после запуска
/// гонится с созданием сокета, поэтому опрашиваем с суммарным лимитом.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);
const SPAWN_POLL: Duration = Duration::from_millis(50);
/// Супервизор: не чаще раза в 2 с и не больше 5 попыток подряд.
pub const RESTART_COOLDOWN: Duration = Duration::from_secs(2);
pub const MAX_CONSECUTIVE_RESTARTS: u32 = 5;

/// События от mpv, уже отвязанные от деталей IPC.
#[derive(Clone, Debug, PartialEq)]
pub enum MpvEvent {
    Position { position: Duration },
    Duration(Duration),
    Paused(bool),
    EndOfFile,
    /// mpv ушёл в idle: плейлист кончился или файл не загрузился.
    Idle,
    /// Супервизор перезапустил упавший процесс. Наружу состояние
    /// падения выставлять обязательно: молча пропавший звук для
    /// человека неотличим от «плеер сломался».
    Restarted,
    /// Лимит перезапусков исчерпан — дальше только ручное вмешательство.
    GaveUp { attempts: u32 },
}

#[derive(Debug, thiserror::Error)]
pub enum MpvError {
    #[error("mpv IPC: {0}")]
    Ipc(String),
    #[error("сокет mpv не появился за {0:?}")]
    SpawnTimeout(Duration),
    #[error("соединение с mpv закрыто")]
    Closed,
    #[error("mpv ответил ошибкой: {0}")]
    Command(String),
}

/// Аргументы запуска mpv. Отдельная функция — её тестироват без сети и
/// без реального процесса.
pub fn mpv_args(socket: &Path, volume: f64) -> Vec<String> {
    vec![
        // idle: mpv не выходит после stop, супервизор не перезапускает
        // его на каждой паузе.
        "--idle=yes".into(),
        "--no-video".into(),
        // --no-ytdl обязателен: URL мы резолвим сами, встроенный
        // ytdl_hook на закрытых треках падает в бот-гейт и не умеет seek
        // по нашим ссылкам.
        "--no-ytdl".into(),
        "--no-terminal".into(),
        format!("--input-ipc-server={}", socket.display()),
        format!("--volume={volume}"),
    ]
}

/// Заголовки для `http-header-fields` перед загрузкой источника.
///
/// Для `Remote` это РОВНО одно значение `User-Agent`. Больше передавать
/// нельзя: mpv разрезает значения опции по запятым, а `Accept` от
/// yt-dlp содержит запятые — запрос распадается на мусорные поля и
/// googlevideo отвечает `400 Bad Request`, хотя тот же URL в curl даёт
/// `206`. Замерено 17.09.2026. Для `Local` заголовки не выставляются
/// вовсе: локальному файлу они не нужны, а лишняя опция — лишний шанс
/// на ту же граблю.
pub fn header_option(source: &StreamSource) -> Option<Vec<String>> {
    match source {
        StreamSource::Local(_) => None,
        StreamSource::Remote {
            user_agent: Some(ua),
            ..
        } => Some(vec![format!("User-Agent: {ua}")]),
        StreamSource::Remote { user_agent: None, .. } => None,
    }
}

/// Сколько ждать подтверждения записи `quit`.
const QUIT_ACK_TIMEOUT: Duration = Duration::from_millis(500);

/// Сколько ждать, пока mpv уйдёт сам после `quit`.
const QUIT_WAIT: Duration = Duration::from_millis(1500);

/// Жив ли процесс. `kill(pid, 0)` ничего не посылает — только проверяет
/// право послать, то есть существование процесса.
fn process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Кадр IPC, уже разобранный по наличию `request_id`.
#[derive(Clone, Debug, PartialEq)]
pub enum IpcFrame {
    Response {
        request_id: u64,
        error: String,
        data: Value,
    },
    Event {
        name: String,
        data: Value,
    },
}

fn parse_frame(line: &str) -> Option<IpcFrame> {
    let value: Value = serde_json::from_str(line).ok()?;
    if let Some(id) = value.get("request_id").and_then(Value::as_u64) {
        let error = value.get("error")?.as_str()?.to_owned();
        Some(IpcFrame::Response {
            request_id: id,
            error,
            data: value.get("data").cloned().unwrap_or(Value::Null),
        })
    } else {
        let name = value.get("event")?.as_str()?.to_owned();
        Some(IpcFrame::Event { name, data: value })
    }
}

/// Секунды mpv → Duration. Отрицательные и нечисловые значения mpv
/// иногда шлёт в момент смены трека — их молча игнорируем, а не паникуем.
fn secs_to_duration(secs: f64) -> Option<Duration> {
    if secs.is_finite() && secs >= 0.0 {
        Some(Duration::from_secs_f64(secs))
    } else {
        None
    }
}

/// Событие IPC → `MpvEvent`.
fn map_event(name: &str, data: &Value) -> Option<MpvEvent> {
    match name {
        "property-change" => {
            let prop = data.get("name")?.as_str()?;
            // Значение всегда в поле `data`, а НЕ в поле, названном по
            // имени свойства. Замерено на живом mpv 0.41:
            //   {"event":"property-change","id":2,"name":"duration","data":266.921}
            // Чтение из `position`/`duration`/`pause`/`eof` давало None на
            // каждом кадре, и наружу не уходили ни позиция, ни длительность,
            // ни пауза, ни конец трека — плеер выглядел молчащим.
            //
            // Поля `data` может не быть вовсе: сразу после
            // `observe_property` mpv присылает кадр без него, когда
            // свойство ещё не определено (нет загруженного файла).
            let value = data.get("data")?;
            match prop {
                "time-pos" => value
                    .as_f64()
                    .and_then(secs_to_duration)
                    .map(|position| MpvEvent::Position { position }),
                "duration" => value
                    .as_f64()
                    .and_then(secs_to_duration)
                    .map(MpvEvent::Duration),
                "pause" => value.as_bool().map(MpvEvent::Paused),
                _ => None,
            }
        }
        "end-file" => match data.get("reason").and_then(Value::as_str) {
            Some("eof") => Some(MpvEvent::EndOfFile),
            _ => None,
        },
        "idle" => Some(MpvEvent::Idle),
        _ => None,
    }
}

enum ConnMsg {
    /// Записать строку и подтвердить, что байты ушли в сокет. Нужно
    /// ровно для `quit` при гашении: без подтверждения демон успевает
    /// выйти раньше, чем `writer_task` доберётся до записи, и mpv
    /// остаётся жить сиротой. Замерено на стенде.
    LineAcked(String, oneshot::Sender<()>),
    /// Подмена потока записи при (пере)запуске mpv.
    SetWriteHalf(tokio::net::unix::OwnedWriteHalf),
    Line(String),
}

struct Inner {
    /// Pid текущего mpv. Обновляется при каждом (пере)запуске.
    /// Нужен, чтобы гашение могло дождаться реальной смерти процесса и
    /// при необходимости добить его: `Child` живёт в супервизоре и
    /// наружу не отдаётся.
    child_pid: std::sync::atomic::AtomicI32,
    /// Демон гасится. Супервизор обязан это видеть: иначе он воспримет
    /// наш `quit` как падение и поднимет mpv заново, а тот переживёт
    /// демон. Замерено на стенде: три прогона оставили пять осиротевших
    /// mpv по ~100 МБ.
    shutting_down: std::sync::atomic::AtomicBool,
    conn: mpsc::Sender<ConnMsg>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, MpvError>>>>,
    events: mpsc::Sender<MpvEvent>,
    next_id: AtomicU64,
}

impl Inner {
    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Дескриптор живого mpv: команды и наблюдение. Клонируется.
#[derive(Clone)]
pub struct Mpv {
    inner: Arc<Inner>,
}

impl Mpv {
    /// Запустить mpv и начать супервизию. Возвращает дескриптор и
    /// поток событий; при смерти процесса соединение и процесс
    /// восстанавливаются самостоятельно, с ограничением частоты.
    pub async fn spawn(
        binary: std::path::PathBuf,
        paths: &Paths,
        volume: f64,
    ) -> Result<(Self, mpsc::Receiver<MpvEvent>), MpvError> {
        let (events_tx, events_rx) = mpsc::channel(64);
        let (conn_tx, conn_rx) = mpsc::channel(64);
        tokio::spawn(writer_task(conn_rx));

        let (child, stream) = spawn_mpv(&binary, paths, volume).await?;
        let inner = Arc::new(Inner {
            conn: conn_tx.clone(),
            pending: Mutex::new(HashMap::new()),
            events: events_tx.clone(),
            next_id: AtomicU64::new(1),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            child_pid: std::sync::atomic::AtomicI32::new(
                child.id().map_or(0, |id| id as i32),
            ),
        });
        let (read, write) = stream.into_split();
        conn_tx
            .send(ConnMsg::SetWriteHalf(write))
            .await
            .map_err(|_| MpvError::Closed)?;
        spawn_reader(read, inner.clone());

        tokio::spawn(supervise(
            child,
            binary,
            paths.clone(),
            volume,
            conn_tx,
            events_tx,
            inner.clone(),
            Instant::now(),
        ));
        Ok((Self { inner }, events_rx))
    }

    /// Погасить mpv вместе с демоном.
    ///
    /// Сначала штатный `quit` по IPC — mpv закрывает вывод и снимает
    /// сокет сам. Ответа не ждём: mpv уходит, не отвечая на `quit`, и
    /// ожидание всегда истекало бы таймаутом.
    pub async fn shutdown(&self) {
        use std::sync::atomic::Ordering;

        self.inner.shutting_down.store(true, Ordering::SeqCst);

        let id = self.inner.alloc_id();
        let line = json!({ "command": ["quit"], "request_id": id }).to_string();
        let (ack, acked) = oneshot::channel();
        if self
            .inner
            .conn
            .send(ConnMsg::LineAcked(line, ack))
            .await
            .is_ok()
        {
            // Ждём именно записи байтов, а не ответа: на `quit` mpv не
            // отвечает — он уходит.
            let _ = tokio::time::timeout(QUIT_ACK_TIMEOUT, acked).await;
        }

        let pid = self.inner.child_pid.load(Ordering::SeqCst);
        if pid <= 0 {
            return;
        }

        // Опрашиваем, пока процесс не исчезнет. `quit` штатно занимает
        // десятки миллисекунд; ждать бесконечно нельзя — залипший mpv
        // не должен задерживать выход демона.
        let deadline = Instant::now() + QUIT_WAIT;
        while Instant::now() < deadline {
            if !process_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Не ушёл сам — добиваем. Оставить его жить нельзя: осиротевший
        // mpv держит ~40 МБ и продолжает играть без всякого управления.
        tracing::warn!(pid, "mpv не ушёл по quit, добиваю");
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }

    /// Наблюдать свойства, из которых собираются `MpvEvent`.
    pub async fn observe(&self) -> Result<(), MpvError> {
        for (id, name) in [(1u64, "time-pos"), (2, "duration"), (3, "pause")] {
            self.request(vec![json!("observe_property"), json!(id), json!(name)])
                .await?;
        }
        Ok(())
    }

    /// Загрузить источник. Для удалённых URL сначала выставляется
    /// единственный заголовок User-Agent (см. `header_option`).
    pub async fn load(&self, source: &StreamSource) -> Result<(), MpvError> {
        if let Some(headers) = header_option(source) {
            self.set_property("http-header-fields", json!(headers)).await?;
        }
        let target = match source {
            StreamSource::Local(path) => path.display().to_string(),
            StreamSource::Remote { url, .. } => url.clone(),
        };
        self.request(vec![json!("loadfile"), json!(target), json!("replace")])
            .await?;
        Ok(())
    }

    pub async fn pause(&self, paused: bool) -> Result<(), MpvError> {
        self.set_property("pause", json!(paused)).await
    }

    pub async fn seek_absolute(&self, position: Duration) -> Result<(), MpvError> {
        self.request(vec![json!("seek"), json!(position.as_secs_f64()), json!("absolute")])
            .await?;
        Ok(())
    }

    pub async fn seek_relative(&self, delta: f64) -> Result<(), MpvError> {
        self.request(vec![json!("seek"), json!(delta), json!("relative")])
            .await?;
        Ok(())
    }

    pub async fn set_volume(&self, volume: f64) -> Result<(), MpvError> {
        self.set_property("volume", json!(volume)).await
    }

    /// Остановить воспроизведение; благодаря `--idle=yes` процесс живёт.
    pub async fn stop(&self) -> Result<(), MpvError> {
        self.request(vec![json!("stop")]).await?;
        Ok(())
    }

    async fn set_property(&self, name: &str, value: Value) -> Result<(), MpvError> {
        self.request(vec![json!("set_property"), json!(name), value])
            .await?;
        Ok(())
    }

    async fn request(&self, command: Vec<Value>) -> Result<Value, MpvError> {
        let id = self.inner.alloc_id();
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, tx);

        let line = json!({ "command": command, "request_id": id }).to_string();
        self.inner
            .conn
            .send(ConnMsg::Line(line))
            .await
            .map_err(|_| MpvError::Closed)?;

        match rx.await {
            Ok(result) => result,
            // Отправитель выброшен: reader умер и разослал ошибки, либо
            // запись в мёртвый сокет не удалась.
            Err(_) => Err(MpvError::Closed),
        }
    }
}

impl Inner {
    /// Разослать всем ждущим командам закрытие соединения. Вызывается
    /// при смерти reader'а: иначе команда висела бы вечно.
    async fn fail_pending(&self) {
        let mut pending = self.pending.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(MpvError::Closed));
        }
    }
}

async fn writer_task(mut rx: mpsc::Receiver<ConnMsg>) {
    let mut out: Option<tokio::net::unix::OwnedWriteHalf> = None;
    while let Some(msg) = rx.recv().await {
        match msg {
            ConnMsg::SetWriteHalf(half) => out = Some(half),
            ConnMsg::Line(line) => {
                let Some(half) = out.as_mut() else { continue };
                if half.write_all(line.as_bytes()).await.is_err()
                    || half.write_all(b"\n").await.is_err()
                {
                    // Сокет умер: обнуляем, следующие команды вернут
                    // Closed через упавшего отправителя.
                    out = None;
                }
            }
            ConnMsg::LineAcked(line, ack) => {
                if let Some(half) = out.as_mut() {
                    if half.write_all(line.as_bytes()).await.is_err()
                        || half.write_all(b"\n").await.is_err()
                        || half.flush().await.is_err()
                    {
                        out = None;
                    }
                }
                // Подтверждаем и при неудаче: вызывающему важно не
                // «получилось», а «ждать больше нечего».
                let _ = ack.send(());
            }
        }
    }
}

/// Цикл чтения одной генерации соединения. Отделяет ответы от событий
/// по наличию `request_id` и умирает вместе с сокетом.
fn spawn_reader(read: tokio::net::unix::OwnedReadHalf, inner: Arc<Inner>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(read).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => match parse_frame(&line) {
                    Some(IpcFrame::Response { request_id, error, data }) => {
                        let tx = inner.pending.lock().await.remove(&request_id);
                        if let Some(tx) = tx {
                            let _ = tx.send(if error == "success" {
                                Ok(data)
                            } else {
                                Err(MpvError::Command(error))
                            });
                        }
                    }
                    Some(IpcFrame::Event { name, data }) => {
                        if let Some(event) = map_event(&name, &data) {
                            let _ = inner.events.send(event).await;
                        }
                    }
                    None => {}
                },
                Ok(None) | Err(_) => break,
            }
        }
        inner.fail_pending().await;
    })
}

async fn spawn_mpv(
    binary: &std::path::Path,
    paths: &Paths,
    volume: f64,
) -> Result<(Child, UnixStream), MpvError> {
    let socket = paths.mpv_socket();
    // Устаревший сокет от прошлого запуска мешает mpv создать новый.
    let _ = std::fs::remove_file(&socket);

    let mut child = Command::new(binary)
        .args(mpv_args(&socket, volume))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            MpvError::Ipc(format!("не удалось запустить {}: {e}", binary.display()))
        })?;

    let stream = wait_for_socket(&socket).await.inspect_err(|_| {
        // Если сокет так и не появился, процесс бесполезен.
        let _ = child.start_kill();
    })?;
    Ok((child, stream))
}

async fn wait_for_socket(socket: &Path) -> Result<UnixStream, MpvError> {
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return Err(MpvError::SpawnTimeout(SPAWN_TIMEOUT));
        }
        match UnixStream::connect(socket).await {
            Ok(stream) => return Ok(stream),
            Err(_) => tokio::time::sleep(SPAWN_POLL).await,
        }
    }
}

/// Супервизор: следит за процессом и перезапускает его с ограничением
/// частоты — не чаще раза в 2 с, не больше 5 попыток подряд. После
/// исчерпания лимита выставляет событие и прекращает попытки.
async fn supervise(
    mut child: Child,
    binary: std::path::PathBuf,
    paths: Paths,
    volume: f64,
    conn: mpsc::Sender<ConnMsg>,
    events: mpsc::Sender<MpvEvent>,
    inner: Arc<Inner>,
    mut last_start: Instant,
) {
    let mut consecutive: u32 = 0;
    loop {
        let _ = child.wait().await;

        // Гасимся — значит процесс ушёл по нашей команде, а не упал.
        // Перезапуск здесь оставил бы mpv жить дольше демона.
        if inner.shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // Соединение предыдущей генерации уже мертво; reader сам разослал
        // ошибки ждущим командам, но подстрахуемся.
        inner.fail_pending().await;

        if consecutive >= MAX_CONSECUTIVE_RESTARTS {
            let _ = events.send(MpvEvent::GaveUp { attempts: consecutive }).await;
            return;
        }

        let since_start = last_start.elapsed();
        if since_start < RESTART_COOLDOWN {
            tokio::time::sleep(RESTART_COOLDOWN - since_start).await;
        }

        match spawn_mpv(&binary, &paths, volume).await {
            Ok((new_child, stream)) => {
                inner.child_pid.store(
                    new_child.id().map_or(0, |id| id as i32),
                    std::sync::atomic::Ordering::SeqCst,
                );
                child = new_child;
                last_start = Instant::now();
                let (read, write) = stream.into_split();
                if conn.send(ConnMsg::SetWriteHalf(write)).await.is_err() {
                    return;
                }
                spawn_reader(read, inner.clone());
                consecutive = 0;
                let _ = events.send(MpvEvent::Restarted).await;
            }
            Err(_) => {
                consecutive += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn remote(ua: Option<&str>) -> StreamSource {
        StreamSource::Remote {
            url: "https://example.invalid/stream".into(),
            user_agent: ua.map(str::to_owned),
            expires_at: Some(SystemTime::now() + Duration::from_secs(3600)),
        }
    }

    #[test]
    fn args_contain_no_ytdl_and_socket() {
        let args = mpv_args(&PathBuf::from("/run/user/1000/tmus-mpv.sock"), 80.0);
        assert!(args.iter().any(|a| a == "--no-ytdl"), "аргументы: {args:?}");
        assert!(args
            .iter()
            .any(|a| a == "--input-ipc-server=/run/user/1000/tmus-mpv.sock"));
        assert!(args.iter().any(|a| a == "--volume=80"));
    }

    #[test]
    fn local_source_gets_no_headers() {
        let source = StreamSource::Local(PathBuf::from("/tmp/a.opus"));
        assert_eq!(header_option(&source), None);
    }

    #[test]
    fn remote_source_gets_exactly_one_user_agent() {
        // Регресс на замеренную граблю: ровно одно значение, без Accept —
        // mpv режет значения по запятым, и запрос распадается.
        let headers = header_option(&remote(Some("Mozilla/5.0"))).expect("заголовки должны быть");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0], "User-Agent: Mozilla/5.0");
    }

    #[test]
    fn remote_without_ua_gets_no_headers() {
        assert_eq!(header_option(&remote(None)), None);
    }

    #[test]
    fn frame_with_request_id_is_response() {
        let frame = parse_frame(r#"{"error":"success","data":1,"request_id":7}"#).expect("кадр должен разбираться");
        match frame {
            IpcFrame::Response { request_id, error, data } => {
                assert_eq!(request_id, 7);
                assert_eq!(error, "success");
                assert_eq!(data, json!(1));
            }
            IpcFrame::Event { .. } => panic!("ответ распознан как событие"),
        }
    }

    #[test]
    fn frame_without_request_id_is_event() {
        let frame = parse_frame(r#"{"event":"idle"}"#).expect("кадр должен разбираться");
        match frame {
            IpcFrame::Event { name, .. } => assert_eq!(name, "idle"),
            IpcFrame::Response { .. } => panic!("событие распознано как ответ"),
        }
    }

    #[test]
    fn property_change_maps_to_events() {
        let pos = map_event(
            "property-change",
            &json!({"event":"property-change","id":1,"name":"time-pos","data":12.5}),
        );
        assert_eq!(pos, Some(MpvEvent::Position { position: Duration::from_millis(12500) }));

        // false не должен изображать конец трека.
        assert_eq!(
            map_event("property-change", &json!({"event":"property-change","id":4,"name":"eof-reached","data":false})),
            None
        );
        assert_eq!(
            map_event("property-change", &json!({"event":"property-change","id":4,"name":"eof-reached","data":true})),
            None
        );

        // Кадр без `data` mpv присылает сразу после `observe_property`,
        // пока свойство не определено (файл ещё не загружен). Он не
        // должен ни падать, ни изображать событие.
        assert_eq!(
            map_event(
                "property-change",
                &json!({"event":"property-change","id":2,"name":"duration"})
            ),
            None
        );
        assert_eq!(map_event("exit", &json!({"event":"exit"})), None);
    }

    #[test]
    fn end_file_reason_eof_maps_to_end_of_file() {
        let eof = map_event(
            "end-file",
            &json!({"event":"end-file","reason":"eof","playlist_entry_id":1}),
        );
        assert_eq!(eof, Some(MpvEvent::EndOfFile));
    }

    #[test]
    fn end_file_other_reasons_do_not_advance() {
        for reason in ["stop", "quit", "error", "redirect", "unknown"] {
            let event = map_event(
                "end-file",
                &json!({"event":"end-file","reason":reason,"playlist_entry_id":1}),
            );
            assert_eq!(event, None, "reason={reason} не должен давать EndOfFile");
        }
        assert_eq!(
            map_event("end-file", &json!({"event":"end-file","playlist_entry_id":1})),
            None,
            "отсутствие reason не должен давать EndOfFile"
        );
    }

    #[test]
    fn negative_time_pos_is_ignored() {
        assert_eq!(secs_to_duration(-0.5), None);
        assert_eq!(secs_to_duration(f64::NAN), None);
        assert_eq!(secs_to_duration(0.0), Some(Duration::ZERO));
    }
}
