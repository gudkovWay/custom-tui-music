use std::sync::Arc;
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
    loop {
        ticker.tick().await;


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
            if let Err(err) = fetch_into_cache(&app, &id).await {
                tracing::warn!(track = %id, %err, "докачка не удалась");
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




/// Скачать трек в офлайн-кэш через тот же резолв, что и воспроизведение.
///
/// URL не кэшируется никогда: у googlevideo он живёт около шести часов
/// (`expire=`). Кэшируется файл.
async fn fetch_into_cache(app: &Arc<App>, id: &TrackId) -> anyhow::Result<()> {
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
    let mut request = reqwest_get(url, user_agent.as_deref()).await?;
    let mut file = tokio::fs::File::create(&partial).await?;
    let mut written: u64 = 0;
    let total = request.content_length();
    let mut last_emit = Instant::now() - PROGRESS_INTERVAL;
    let mut last_relevance = Instant::now();
    while let Some(chunk) = request.chunk().await? {
        use tokio::io::AsyncWriteExt as _;
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;

        let now = Instant::now();
        if should_emit(last_emit, now, false) {
            last_emit = now;
            app.emit(Event::CacheProgress { track: id.clone(), bytes: written, total });
        }

        // Человек ушёл с трека — докачивать до конца бессмысленно: это лишние
        // трафик, диск и CPU на события. Проверяем актуальность не чаще раза
        // в секунду, чтобы не дёргать state()/with_queue на каждом chunk'е.
        if now.duration_since(last_relevance) >= RELEVANCE_INTERVAL {
            last_relevance = now;
            let still_wanted = {
                let state = app.player.state().await;
                let current = state.track.as_ref().map(|t| &t.id) == Some(id);
                let next = app
                    .player
                    .with_queue(|q| q.peek_next().map(|t| t.id.clone()))
                    .await
                    .as_ref() == Some(id);
                current || next
            };
            if !still_wanted {
                drop(file);
                if let Err(err) = tokio::fs::remove_file(&partial).await {
                    tracing::debug!(%err, "не удалось удалить .part отменённой докачки");
                }
                tracing::debug!("докачка отменена: трек вне окна префетча");
                return Ok(());
            }
        }
    }
    use tokio::io::AsyncWriteExt as _;
    app.emit(Event::CacheProgress { track: id.clone(), bytes: written, total });
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&partial, &target).await?;

    app.with_cache(|c| c.register_audio(id, &target, ext))?;
    let _ = expires_at;
    Ok(())
}

/// Загрузка с ровно одним заголовком.
///
/// Передаём только `User-Agent`, как и в mpv: остальные заголовки
/// yt-dlp содержат запятые, и mpv их разрезает, отчего googlevideo
/// отвечает `400`. Здесь разрезать нечему, но набор заголовков держим
/// одинаковым — иначе кэш и воспроизведение расходились бы в том, что
/// именно сервер считает валидным запросом.
/// Общий HTTP-клиент загрузки. Таймауты обязательны: зависший коннект к
/// googlevideo без них вешает филлер навсегда — останавливаются и gc, и
/// докачка очереди. Зависший chunk хуже ошибки: 120 с — на трек целиком,
/// а не на один chunk; connect_timeout рано отсеивает мёртвые сети. В
/// innertube.rs оба таймаута уже стоят по той же причине.

async fn reqwest_get(url: &str, user_agent: Option<&str>) -> anyhow::Result<reqwest::Response> {
    let client = reqwest::Client::builder().build()?;
    let mut request = client.get(url);
    if let Some(ua) = user_agent {
        request = request.header(reqwest::header::USER_AGENT, ua);
    }
    let response = request.send().await?.error_for_status()?;
    Ok(response)
}



