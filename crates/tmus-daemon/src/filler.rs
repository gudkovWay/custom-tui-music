use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime};

use tmus_core::model::TrackId;
use tmus_core::protocol::Event;

use crate::app::App;

#[cfg(test)]
mod progress_throttle_tests {
    use std::time::{Duration, Instant};

    use super::{should_emit, PROGRESS_INTERVAL};

    #[test]
    fn interval_boundary_and_final_frame() {
        let t0 = Instant::now();
        // Раньше интервала — молчим.
        assert!(!should_emit(t0, t0 + PROGRESS_INTERVAL - Duration::from_millis(1), false));
        // Ровно интервал — пора.
        assert!(should_emit(t0, t0 + PROGRESS_INTERVAL, false));
        // Завершающий кадр уходит всегда, даже раньше интервала.
        assert!(should_emit(t0, t0, true));
    }
}

/// Сколько треков вперёд докачивает фоновый филлер. Глубина 3, а не 1:
/// скип-серия и пара треков вперёд не должны натыкаться на ~5-секундный
/// yt-dlp (замер 18.09: закэшированный старт — 8 мс). Дальше трёх —
/// лишний трафик на дальний прогноз при смене настроения слушателя.
const PREFETCH_DEPTH: usize = 3;
/// Фоновая докачка в офлайн-кэш.
///
/// Хозяин заказал офлайн явно, и «скачать по требованию» его не закрывает:
/// нужен трек, который уже слушали. Поэтому играемый трек кладётся на
/// диск, а следующий в очереди докачивается заранее.
pub async fn run_cache_filler(app: Arc<App>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ticks: u64 = 0;
    // Состояние неудач по трекам между тиками. Без него филлер долбил
    // мёртвый трек каждые 5 с (вечер 20.09: один трек — 91 попытка за
    // 5 минут), каждый раз поднимая yt-dlp (~0.9 CPU-с, до 335 МБ).
    let mut retries: std::collections::HashMap<TrackId, RetryState> = Default::default();
    loop {
        ticker.tick().await;
        ticks += 1;

        // Автозачекпойнт WAL срабатывает редко, и файл растёт без нужды;
        // раз в минуту усекаем его принудительно. Ошибка не должна
        // ломать цикл докачки.
        if ticks % 12 == 0 {
            if let Err(err) = app.with_cache(|c| c.checkpoint_wal()) {
                tracing::warn!(%err, "WAL-чекпойнт не удался");
            }
        }

        let wanted = {
            let state = app.player.state().await;
            let current = state.track.map(|t| t.id);
            // Глубина 3, а не 1: скип-серия и просто пара треков вперёд
            // не должны натыкаться на ~5-секундный yt-dlp (замер:
            // закэшированный старт 8 мс). Дальше трёх — лишний трафик
            // на дальний прогноз при смене настроения слушателя.
            let ahead = app
                .player
                .with_queue(|q| q.peek_ahead(PREFETCH_DEPTH).into_iter().map(|t| t.id).collect::<Vec<_>>())
                .await;
            let mut wanted: Vec<TrackId> = Vec::with_capacity(1 + ahead.len());
            wanted.extend(current);
            wanted.extend(ahead);
            wanted
        };

        for id in wanted {
            match app.with_cache(|c| c.lookup_audio(&id)) {
                Ok(Some(_)) => continue,
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(%err, "кэш не опрашивается");
                    continue;
                }
            }
            // Кулдаун: трек недавно провалился — пропускаем без сетевых
            // попыток, чтобы не плодить процессы yt-dlp впустую.
            if retries.get(&id).is_some_and(|s| s.in_cooldown(Instant::now())) {
                tracing::debug!(track = %id, "трек в кулдауне бэкоффа, пропускаю");
                continue;
            }
            // Невоспроизводимый трек не в кулдауне, а выбыл совсем:
            // лестница 1м→5м→15м→1ч для постоянной причины — просто
            // отложенные впустую запуски yt-dlp.
            if retries.get(&id).is_some_and(|s| s.unplayable) {
                continue;
            }
            if let Err(err) = fetch_into_cache(&app, &id).await {
                // `Unplayable` — терминальный исход этого трека:
                // записываем раз, логируем и идём к следующему
                // кандидату через обычный поток филлера; бэкофф не
                // назначаем.
                if err
                    .downcast_ref::<tmus_provider::ProviderError>()
                    .is_some_and(|e| matches!(e, tmus_provider::ProviderError::Unplayable { .. }))
                {
                    tracing::warn!(track = %id, %err, "трек нельзя воспроизвести — снимаю с докачки");
                    retries.entry(id.clone()).or_default().record_unplayable();
                    continue;
                }
                let now = Instant::now();
                let state = retries.entry(id.clone()).or_default();
                let escalated = state.record_failure(now);
                let delay = next_delay(state.failures);
                if escalated {
                    // warn только на смену уровня бэкоффа: повтор внутри
                    // уровня — заведомо та же причина, шуметь нечего.
                    tracing::warn!(track = %id, attempts = state.failures, retry_in = ?delay, %err, "докачка не удалась, увеличиваю паузу");
                } else {
                    tracing::debug!(track = %id, attempts = state.failures, retry_in = ?delay, %err, "докачка не удалась, жду бэкофф");
                }
            } else {
                retries.entry(id.clone()).or_default().record_success();
            }
        }

        if let Err(err) = app.with_cache(|c| c.gc()) {
            tracing::warn!(%err, "вытеснение кэша не удалось");
        }
    }
}

// Троттлинг прогресса: на chunk'ах по 8 КиБ событие шины уходило на каждый
// chunk — замерено 490 ev/s (183 события за 0.37 с на файле 3 МБ), а буфер
// шины всего 256, подписчики уходят в Lagged. Шкала быстрее 2 Гц всё равно
// никому не видна.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

// Проверка актуальности трека стоит обращений к state()/with_queue, поэтому
// не на каждый chunk, а не чаще раза в секунду.
const RELEVANCE_INTERVAL: Duration = Duration::from_secs(1);

/// Пора ли слать прогресс: либо прошло не меньше интервала, либо это
/// завершающий кадр — он обязан уйти всегда, иначе потребитель не увидит 100%.
fn should_emit(last: Instant, now: Instant, done: bool) -> bool {
    done || now.duration_since(last) >= PROGRESS_INTERVAL
}

/// Актуален ли трек для докачки: текущий или в окне префетча.
fn still_wanted(id: &TrackId, current: Option<&TrackId>, ahead: &[TrackId]) -> bool {
    current == Some(id) || ahead.iter().any(|a| a == id)
}

/// Задержка перед следующей попыткой после `failures` неудач подряд.
/// Шкала снята с вечера 20.09: без бэкоффа филлер за 5 минут долбил один
/// мёртвый трек 91 раз (314 процессов yt-dlp за вечер, ~0.9 CPU-с и до
/// 335 МБ каждый). Индекс = число неудач: 0 неудач — можно сразу (0),
/// после 1-й — минута, после 2-й — 5 минут, после 3-й — 15, дальше
/// потолок час: трек скорее всего выпилен, но через час всё же переспросим.
fn next_delay(failures: u32) -> Duration {
    match failures {
        0 => Duration::ZERO,
        1 => Duration::from_secs(60),
        2 => Duration::from_secs(5 * 60),
        3 => Duration::from_secs(15 * 60),
        _ => Duration::from_secs(60 * 60),
    }
}

/// Состояние неудач по одному треку; живёт между тиками филлера
/// в локальной HashMap (не в статике — статика с мьютексом тут лишняя,
/// состояние нужно только одному потоку филлера).
#[derive(Default)]
struct RetryState {
    failures: u32,
    retry_after: Option<Instant>,
    /// Причина отказа постоянная (`Unplayable`: DRM, отрезанный стрим):
    /// трек не на лестнице бэкоффа, а вовсе вне докачки. Повтор через
    /// час ничего не изменит — только новый впустую запущенный yt-dlp.
    unplayable: bool,
}

impl RetryState {
    /// Трек в кулдауне — пропускаем без сетевых попыток вовсе.
    fn in_cooldown(&self, now: Instant) -> bool {
        self.retry_after.is_some_and(|t| now < t)
    }

    /// Трек признан невоспроизводимым: терминальный исход, вне лестницы.
    fn record_unplayable(&mut self) {
        self.unplayable = true;
        self.retry_after = None;
    }

    /// Фиксируем неудачу; возвращает true, если сменился уровень бэкоффа
    /// (пора warn вместо debug). Уровни соответствуют next_delay.
    fn record_failure(&mut self, now: Instant) -> bool {
        self.failures += 1;
        self.retry_after = Some(now + next_delay(self.failures));
        matches!(self.failures, 1 | 2 | 3 | 4)
    }

    /// Успех (или отмена вне окна) сбрасывает серию: трек скачался —
    /// наказывать его за прошлые глюки сети нечего.
    fn record_success(&mut self) {
        self.failures = 0;
        self.retry_after = None;
        self.unplayable = false;
    }
}

/// `clen` из query googlevideo-URL — точный размер файла в байтах.
/// Мусор вместо числа и отсутствие параметра — одно и то же: ожидаемого
/// размера нет.
fn clen_from_url(url: &str) -> Option<u64> {
    reqwest::Url::parse(url)
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == "clen")
        .and_then(|(_, v)| v.parse().ok())
}

/// Снимок окна докачки: текущий трек и хвост очереди. Один хелпер для
/// всех проверок актуальности — филлер и прогрев обязаны сверяться с
/// одним и тем же окном (глубина = [`PREFETCH_DEPTH`]).
async fn relevance_snapshot(app: &Arc<App>) -> (Option<TrackId>, Vec<TrackId>) {
    let state = app.player.state().await;
    let current = state.track.map(|t| t.id);
    let ahead = app
        .player
        .with_queue(|q| q.peek_ahead(PREFETCH_DEPTH).into_iter().map(|t| t.id).collect::<Vec<_>>())
        .await;
    (current, ahead)
}

#[cfg(test)]
mod relevance_tests {
    use tmus_core::model::{ProviderId, TrackId};

    use super::still_wanted;

    fn id(s: &str) -> TrackId {
        TrackId::new(ProviderId::YTMUSIC, s)
    }

    #[test]
    fn current_track_is_wanted() {
        let cur = id("cur");
        assert!(still_wanted(&cur, Some(&cur), &[]));
    }

    #[test]
    fn track_two_ahead_is_wanted() {
        // Позиция +2 в окне префетча: именно такие треки раньше отменялись
        // сверкой только с текущим и следующим.
        let ahead = vec![id("next"), id("next2"), id("next3")];
        let target = id("next2");
        assert!(still_wanted(&target, Some(&id("cur")), &ahead));
    }

    #[test]
    fn unrelated_track_is_not_wanted() {
        let ahead = vec![id("next"), id("next2")];
        let stranger = id("stranger");
        assert!(!still_wanted(&stranger, Some(&id("cur")), &ahead));
    }

    #[test]
    fn empty_player_is_not_wanted() {
        let target = id("cur");
        assert!(!still_wanted(&target, None, &[]));
    }
}

/// Скачать трек в офлайн-кэш через тот же резолв, что и воспроизведение.
///
/// URL не кэшируется никогда: у googlevideo он живёт около шести часов
/// (`expire=`). Кэшируется файл.
async fn fetch_into_cache(app: &Arc<App>, id: &TrackId) -> anyhow::Result<()> {
    // Семафор резолвов один на процесс: параллельные yt-dlp не имеют
    // смысла (0.9 CPU-с и 335 МБ на процесс), а филлер без него обгонял
    // плеер и запускал второй yt-dlp параллельно. Пока ждём очередь,
    // трек мог выпасть из окна префетча — поэтому ждём с периодической
    // проверкой актуальности: устаревшее место в очереди освобождаем,
    // а не держим.
    // Гейт вынесен за цикл: `acquire()` держит ссылку на семафор, и
    // временный `Arc` из геттера внутри `select!` жил бы только до конца
    // итерации.
    let gate = app.resolve_gate();
    let permit = loop {
        tokio::select! {
            biased;
            permit = gate.acquire() =>
                break permit.expect("семафор резолвов живёт столько же, сколько демон"),
            _ = tokio::time::sleep(RELEVANCE_INTERVAL) => {
                let (current, ahead) = relevance_snapshot(app).await;
                if !still_wanted(id, current.as_ref(), &ahead) {
                    tracing::debug!("докачка отменена в очереди резолвов: трек вне окна префетча");
                    return Ok(());
                }
            }
        }
    };
    let _permit = permit;

    let resolver = app.registry.resolver_for(id)?;
    let source = resolver.resolve(id).await?;
    let tmus_core::model::StreamSource::Remote { url, user_agent, expires_at } = &source else {
        // Уже локальный — значит кэш опередил нас, докачивать нечего.
        return Ok(());
    };
    if source.is_expired(SystemTime::now()) {
        anyhow::bail!("резолв истёк до начала загрузки");
    }

    let ext = "webm";
    let target = app.with_cache(|c| c.audio_path(id, ext));
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Пишем в соседний файл и переименовываем: оборванная загрузка не
    // должна попасть в кэш как готовый трек — она игралась бы обрезанной.
    let partial = target.with_extension(format!("{ext}.part"));
    let (mut response, expected, ranged) = open_download(url, user_agent.as_deref()).await?;
    let mut file = tokio::fs::File::create(&partial).await?;
    let mut written: u64 = 0;
    let mut last_emit = Instant::now() - PROGRESS_INTERVAL;
    let mut last_relevance = Instant::now();
    loop {
        // Чанков в текущем ответе (range-режим — один HTTP-запрос на
        // чанк): короткое тело без clen — признак конца файла.
        let mut served: u64 = 0;
        while let Some(chunk) = response.chunk().await? {
            use tokio::io::AsyncWriteExt as _;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
            served += chunk.len() as u64;

            let now = Instant::now();
            if should_emit(last_emit, now, false) {
                last_emit = now;
                app.emit(Event::CacheProgress { track: id.clone(), bytes: written, total: expected });
            }

            // Человек ушёл с трека — докачивать до конца бессмысленно: это лишние
            // трафик, диск и CPU на события. Проверяем актуальность не чаще раза
            // в секунду, чтобы не дёргать state()/with_queue на каждом chunk'е.
            // Глубина сверки совпадает с глубиной префетча: сверка только с
            // текущим и следующим отменяла треки на +2/+3 сразу после первого
            // RELEVANCE_INTERVAL, а рестарт тика качал их заново — по yt-dlp
            // (~0.9 CPU-с, пик 335 МБ) и трафику на каждый круг.
            if now.duration_since(last_relevance) >= RELEVANCE_INTERVAL {
                last_relevance = now;
                let wanted = {
                    let (current, ahead) = relevance_snapshot(app).await;
                    still_wanted(id, current.as_ref(), &ahead)
                };
                if !wanted {
                    drop(file);
                    if let Err(err) = tokio::fs::remove_file(&partial).await {
                        tracing::debug!(%err, "не удалось удалить .part отменённой докачки");
                    }
                    tracing::debug!("докачка отменена: трек вне окна префетча");
                    return Ok(());
                }
            }
        }
        // Конец: plain-режим — тело исчерпано; range — скачали clen или
        // сервер отдал короткий/пустой чанк (последний кусок файла).
        if !ranged {
            break;
        }
        if expected.is_some_and(|total| written >= total) || served == 0 {
            break;
        }
        if expected.is_none() && served < RANGE_CHUNK {
            break;
        }
        response = range_chunk(url, user_agent.as_deref(), written, expected).await?;
    }
    use tokio::io::AsyncWriteExt as _;

    // Размер — единственная дешёвая проверка целостности: googlevideo
    // даёт clen в query, и обрезанный/подменённый ответ обязан не
    // совпасть. Без clen верим Content-Length; нет ни того, ни другого —
    // проверить нечем, принимаем как есть (кэш-плеер об ошибке скажет сам).
    if let Some(total) = expected {
        if written != total {
            drop(file);
            if let Err(err) = tokio::fs::remove_file(&partial).await {
                tracing::debug!(%err, "не удалось удалить .part несовпавшего размера");
            }
            anyhow::bail!("размер не совпал: скачано {written} из {total}");
        }
    }
    file.flush().await?;
    drop(file);
    // HLS — это список сегментов, не файл трека. Собираем аудио без
    // перекодирования до публикации; отмена и таймаут не регистрируют .part.
    let prepared = {
        let prepare = prepare_cached_audio(&partial, url, user_agent.as_deref());
        tokio::pin!(prepare);
        loop {
            tokio::select! {
                result = &mut prepare => break result.map(Some),
                _ = tokio::time::sleep(RELEVANCE_INTERVAL) => {
                    let (current, ahead) = relevance_snapshot(app).await;
                    if !still_wanted(id, current.as_ref(), &ahead) {
                        break Ok(None);
                    }
                }
            }
        }
    };
    let (ext, written) = match prepared {
        Ok(Some(audio)) => audio,
        result => {
            let _ = tokio::fs::remove_file(&partial).await;
            return result.map(|_| ());
        }
    };
    let target = target.with_extension(ext);
    tokio::fs::rename(&partial, &target).await?;

    app.with_cache(|c| c.register_audio(id, &target, ext))?;
    app.emit(Event::CacheProgress { track: id.clone(), bytes: written, total: Some(written) });
    let _ = expires_at;
    Ok(())
}

async fn prepare_cached_audio(
    partial: &std::path::Path,
    url: &str,
    user_agent: Option<&str>,
) -> anyhow::Result<(&'static str, u64)> {
    if !tmus_core::cache::is_audio_manifest(partial)? {
        return Ok(("webm", tokio::fs::metadata(partial).await?.len()));
    }
    let mut command = tokio::process::Command::new("ffmpeg");
    command.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y",
        "-rw_timeout", "10000000"]);
    if let Some(ua) = user_agent {
        command.args(["-user_agent", ua]);
    }
    // Matroska допускает AAC, MP3 и Opus; WebM не допускает AAC.
    // ffmpeg читает исходный URL, чтобы относительные ссылки HLS работали.
    command.args(["-i", url, "-map", "0:a:0", "-vn", "-c:a", "copy", "-f", "matroska"])
        .arg(partial)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let status = match tokio::time::timeout(Duration::from_secs(120), child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!("загрузка HLS не завершилась за 120с");
        }
    };
    anyhow::ensure!(status.success(), "ffmpeg не смог скачать HLS: {status}");
    let bytes = tokio::fs::metadata(partial).await?.len();
    anyhow::ensure!(bytes > 0, "пустое аудио после загрузки HLS");
    Ok(("mka", bytes))
}

#[cfg(test)]
mod hls_cache_tests {
    use super::prepare_cached_audio;

    #[tokio::test]
    async fn hls_cache_contains_audio_without_original_segments() {
        assert_hls_cache("mpegts", "ts").await;
    }

    #[tokio::test]
    async fn hls_cache_handles_fragmented_mp4() {
        assert_hls_cache("fmp4", "m4s").await;
    }

    async fn assert_hls_cache(segment_type: &str, extension: &str) {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let manifest = source.join("index.m3u8");
        let generated = tokio::process::Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-f", "lavfi",
                "-i", "sine=frequency=440:sample_rate=44100", "-t", "1", "-threads", "1",
                "-c:a", "aac", "-f", "hls", "-hls_segment_type", segment_type,
                "-hls_segment_filename"])
            .arg(source.join(format!("segment%03d.{extension}")))
            .arg(&manifest)
            .status().await.unwrap();
        assert!(generated.success());
        let valid_fixture = tokio::process::Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(&manifest).args(["-f", "null", "-"]).status().await.unwrap();
        assert!(valid_fixture.success(), "HLS fixture must decode before caching");
        let partial = dir.path().join("audio.part");
        std::fs::copy(&manifest, &partial).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/index.m3u8", listener.local_addr().unwrap());
        let fixture = source.clone();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                let n = socket.read(&mut request).await.unwrap();
                let request = std::str::from_utf8(&request[..n]).unwrap();
                assert!(request.contains("User-Agent: tmus-hls-test\r\n"));
                let path = request.split_whitespace().nth(1).unwrap();
                let name = match path {
                    "/index.m3u8" => "index.m3u8",
                    "/segment000.ts" => "segment000.ts",
                    "/segment000.m4s" => "segment000.m4s",
                    "/init.mp4" => "init.mp4",
                    _ => panic!("unexpected HLS request: {path}"),
                };
                let body = std::fs::read(fixture.join(name)).unwrap();
                let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        });

        let result = prepare_cached_audio(&partial, &url, Some("tmus-hls-test")).await;
        server.abort();
        let _ = server.await;
        let (ext, bytes) = result.unwrap();
        assert_eq!(ext, "mka");
        assert_eq!(bytes, std::fs::metadata(&partial).unwrap().len());
        assert!(bytes > 0);
        std::fs::remove_dir_all(&source).unwrap();
        let decoded = tokio::process::Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(&partial)
            .args(["-f", "null", "-"])
            .status().await.unwrap();
        assert!(decoded.success(), "кэш обязан играть без исходного плейлиста и сегментов");
    }

    #[tokio::test]
    async fn hls_cache_preserves_direct_audio_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("audio.part");
        let audio = b"ID3direct audio bytes";
        std::fs::write(&partial, audio).unwrap();
        let (ext, bytes) = prepare_cached_audio(&partial, "unused", None).await.unwrap();
        assert_eq!(ext, "webm");
        assert_eq!(bytes, audio.len() as u64);
        assert_eq!(std::fs::read(partial).unwrap(), audio);
    }

    #[tokio::test]
    async fn hls_cache_reports_download_failure() {
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("audio.part");
        std::fs::write(&partial, b"#EXTM3U\n#EXT-X-VERSION:7\n").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/missing.m3u8", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await.unwrap();
        });
        let result = prepare_cached_audio(&partial, &url, None).await;
        server.abort();
        let _ = server.await;
        assert!(result.is_err());
    }
}

/// Заголовок запроса — ровно один, `User-Agent`, как и у mpv:
/// остальные заголовки yt-dlp содержат запятые, и mpv их разрезает,
/// отчего googlevideo отвечает `400`. Здесь разрезать нечему, но набор
/// заголовков держим одинаковым — иначе кэш и воспроизведение
/// расходились бы в том, что именно сервер считает валидным запросом.
/// Общий HTTP-клиент загрузки. Таймауты обязательны: зависший коннект к
/// googlevideo без них вешает филлер навсегда — останавливаются и gc, и
/// докачка очереди. Зависший chunk хуже ошибки: 120 с — на трек целиком,
/// а не на один chunk; connect_timeout рано отсеивает мёртвые сети. В
/// innertube.rs оба таймаута уже стоят по той же причине.
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()
        // Из-за одних таймаутов билдер не падает; паника здесь сигнализирует
        // о сломанном TLS-бэкенде, а не о состоянии сети.
        .expect("reqwest client")
});

/// Чанк range-загрузки. Ровно 1 MiB — это не про вкусовщину: замер
/// 21.09, `Range: bytes=0-<end>`, окно ≤1 MiB → 206, 4 MiB → 403;
/// googlevideo для ссылок `c=WEB_REMIX` режет диапазоны больше 1 MiB.
const RANGE_CHUNK: u64 = 1024 * 1024;

/// Открыть загрузку: обычный GET, а при 4xx — range-режим.
///
/// С волны 403 от googlevideo (20.09) обычный GET без Range для ссылок
/// `c=WEB_REMIX` отдаёт 403, хотя mpv (который тянет диапазонами) играет
/// тот же URL. Диапазоны по 1 MiB (см. [`RANGE_CHUNK`]) сервер
/// стабильно отвечает 206, поэтому провал обычного GET на 4xx — не
/// «трек мёртв», а сигнал качать диапазонами. Второй GET делаем тем же
/// URL: сами ссылки уникальны для клиента, Range к ним добавляет только
/// заголовок.
///
/// Возвращает (ответ, ожидаемый размер, range-режим): в plain-режиме
/// размер — clen из URL, иначе Content-Length; в range-режиме — clen
/// (если параметра нет, качаем последовательно до короткого чанка и
/// ожидаемого размера нет).
async fn open_download(
    url: &str,
    user_agent: Option<&str>,
) -> anyhow::Result<(reqwest::Response, Option<u64>, bool)> {
    let clen = clen_from_url(url);
    let response = send_get(url, user_agent, None).await?;
    let status = response.status();
    if !status.is_client_error() {
        let response = response.error_for_status()?;
        let expected = clen.or(response.content_length());
        return Ok((response, expected, false));
    }
    // 4xx: пробуем range-режим. Диапазоны считаем от clen, если он есть;
    // иначе идём последовательно до короткого чанка — ожидаемый размер
    // тогда определят уже по факту скачивания.
    tracing::debug!(%status, "обычный GET отклонён, переключаюсь на range-режим");
    let response = send_get(url, user_agent, Some(0..RANGE_CHUNK - 1)).await?;
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        // Сервер ответил на Range полным 200 — тело тоже валидно, качаем
        // его целиком, как plain.
        let expected = clen.or(response.content_length());
        return Ok((response.error_for_status()?, expected, false));
    }
    let ranged = true;
    Ok((response, clen, ranged))
}

/// GET с единственным заголовком UA (см. [`HTTP_CLIENT`]) плюс
/// опциональный Range (включительно, по RFC 9110).
async fn send_get(
    url: &str,
    user_agent: Option<&str>,
    range: Option<std::ops::Range<u64>>,
) -> anyhow::Result<reqwest::Response> {
    let mut request = HTTP_CLIENT.get(url);
    if let Some(ua) = user_agent {
        request = request.header(reqwest::header::USER_AGENT, ua);
    }
    if let Some(range) = range {
        request = request.header(reqwest::header::RANGE, format!("bytes={}-{}", range.start, range.end));
    }
    let response = request.send().await?;
    Ok(response)
}

/// Следующий range-ответ: 1 MiB с `start` (или до конца файла).
async fn range_chunk(
    url: &str,
    user_agent: Option<&str>,
    start: u64,
    expected: Option<u64>,
) -> anyhow::Result<reqwest::Response> {
    let end = match expected {
        Some(total) => (start + RANGE_CHUNK).saturating_sub(1).min(total - 1),
        None => start + RANGE_CHUNK - 1,
    };
    send_get(url, user_agent, Some(start..end)).await?.error_for_status().map_err(Into::into)
}

/// Фоновый прогрев кэша: докачать список треков, пропуская готовое.
/// Прогресс виден потоком `CacheProgress` (как у фоновой докачки).
///
/// Ошибка одного трека (сеть, возраст, выпиленное видео) не останавливает
/// прогрев: warn и дальше — заказчик греет плейлист целиком, а не один
/// конкретный трек. Дедуп с сохранением порядка: плейлисты бывают с
/// повторами, качать один трек дважды незачем.
pub async fn warm(app: Arc<App>, tracks: Vec<TrackId>) -> anyhow::Result<()> {
    let mut seen = std::collections::HashSet::new();
    let tracks: Vec<TrackId> = tracks
        .into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect();
    let total = tracks.len();
    let mut done: usize = 0;
    for id in &tracks {
        match app.with_cache(|c| c.lookup_audio(id)) {
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(track = %id, %err, "кэш не опрашивается при прогреве");
                continue;
            }
        }
        match fetch_into_cache(&app, id).await {
            Ok(()) => done += 1,
            Err(err) => tracing::warn!(track = %id, %err, "прогрев трека не удался"),
        }
    }
    tracing::info!(успешно = done, всего = total, "прогрев кэша завершён");
    Ok(())
}



#[cfg(test)]
mod backoff_tests {
    use std::time::{Duration, Instant};

    use super::{clen_from_url, next_delay, RetryState};

    #[test]
    fn delay_ladder_matches_recorded_scale() {
        // Шкала 21.09 (см. next_delay): 0 неудач — сразу, дальше минута,
        // 5, 15 и потолок час.
        assert_eq!(next_delay(0), Duration::ZERO);
        assert_eq!(next_delay(1), Duration::from_secs(60));
        assert_eq!(next_delay(2), Duration::from_secs(5 * 60));
        assert_eq!(next_delay(3), Duration::from_secs(15 * 60));
        assert_eq!(next_delay(4), Duration::from_secs(60 * 60));
        assert_eq!(next_delay(10), Duration::from_secs(60 * 60));
    }

    #[test]
    fn cooldown_blocks_until_retry_after() {
        let now = Instant::now();
        let mut state = RetryState::default();
        assert!(!state.in_cooldown(now), "без неудач кулдауна нет");
        state.record_failure(now);
        assert!(state.in_cooldown(now + Duration::from_secs(1)));
        assert!(!state.in_cooldown(now + next_delay(1)), "после retry_after кулдаун истёк");
    }

    #[test]
    fn success_resets_series() {
        let now = Instant::now();
        let mut state = RetryState::default();
        state.record_failure(now);
        state.record_failure(now);
        assert!(state.in_cooldown(now));
        state.record_success();
        assert!(!state.in_cooldown(now), "успех снимает кулдаун");
        state.record_failure(now);
        // Серия началась заново: минута, а не 5/15 минут.
        assert_eq!(next_delay(state.failures), Duration::from_secs(60));
    }

    #[test]
    fn escalation_flags_only_level_changes() {
        // warn только на первую неудачу и смену уровня (1..=4), повторы
        // внутри часового потолка — debug.
        let now = Instant::now();
        let mut state = RetryState::default();
        for attempt in 1..=6 {
            let escalated = state.record_failure(now);
            assert_eq!(escalated, (1..=4).contains(&attempt), "attempt={attempt}");
        }
    }

    #[test]
    fn clen_is_parsed_from_query() {
        assert_eq!(clen_from_url("https://rr1---sn.example.googlevideo.com/videoplayback?id=x&clen=1234567&expire=42"), Some(1_234_567));
        assert_eq!(clen_from_url("https://example.com/videoplayback?id=x"), None, "clen нет");
        assert_eq!(clen_from_url("https://example.com/videoplayback?clen=garbage"), None, "clen-мусор");
        assert_eq!(clen_from_url("не url вовсе"), None);
    }
}
