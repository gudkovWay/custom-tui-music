//! Персист очереди: queue.json рядом со стейтом каталога.
//!
//! Зачем: state.json хранит только настройки (36 байт), и рестарт демона
//! стирал очередь — замер 18.09. Здесь очередь+позиция живут отдельным
//! файлом, демонт стартует с восстановленной очередью, но БЕЗ автоплея:
//! mpv по-прежнему молчит, пока человек не нажмёт play.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tmus_core::model::{LoopMode, Track};
use tmus_core::paths::Paths;
use tmus_core::protocol::Event;

use crate::app::App;

/// Имя файла очереди в каталоге стейта (`~/.local/state/tmus/`).
const QUEUE_FILE: &str = "queue.json";

/// Снапшот очереди. Формат стабильный и плоский: треки в исходном
/// порядке, `index` — позиция в этом же порядке (при shuffle это индекс
/// в векторе треков, а не в перестановке — его и восстанавливает
/// `Queue::restore`).
#[derive(Serialize, Deserialize)]
struct PersistState {
    queue: Vec<Track>,
    index: Option<usize>,
    loop_mode: LoopMode,
    shuffle: bool,
}

/// Путь файла очереди. Каталог тот же, куда пишет
/// `tmus_core::catalog_source::save`, — два стейта в разных местах
/// разъехались бы при бэкапе.
fn queue_file(paths: &Paths) -> std::path::PathBuf {
    paths.state_dir().join(QUEUE_FILE)
}

/// Записать снапшот очереди немедленно. Атомарно: tmp-файл + rename,
/// как у `catalog_source::save` — оборванная запись не должна оставлять
/// после себя битый queue.json.
pub async fn flush_now(app: &App) -> anyhow::Result<()> {
    let snapshot = app
        .player()
        .with_queue(|q| (q.tracks().to_vec(), q.current_index(), q.loop_mode(), q.shuffle()))
        .await;
    let state = PersistState {
        queue: snapshot.0,
        index: snapshot.1,
        loop_mode: snapshot.2,
        shuffle: snapshot.3,
    };
    let file = queue_file(app.paths());
    let tmp = file.with_extension("json.tmp");
    std::fs::create_dir_all(app.paths().state_dir())?;
    std::fs::write(
        &tmp,
        serde_json::to_vec(&state).map_err(|err| std::io::Error::other(err.to_string()))?,
    )?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

/// Фоновая запись: очередь меняется событиями шины, а не по таймеру,
/// поэтому пишем по грязному флагу раз в 5 с — Position идёт каждую
/// секунду, и писать на каждый тик означало бы бессмысленный износ диска.
pub async fn run(app: Arc<App>) {
    let mut events = app.subscribe();
    let mut dirty = false;
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(Event::QueueChanged { .. } | Event::TrackChanged { .. }) => dirty = true,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            _ = tick.tick() => {
                if dirty {
                    dirty = false;
                    // Ошибка записи не валит демон: очередь дороже
                    // пережить, чем уронить воспроизведение.
                    if let Err(err) = flush_now(&app).await {
                        tracing::warn!(%err, "очередь не записалась");
                    }
                }
            }
        }
    }
}

/// Восстановить очередь при старте. Отсутствие файла или битый JSON —
/// тихий debug: первый запуск и повреждённый файл — штатные случаи,
/// демон ОБЯЗАН стартовать в любом из них.
pub async fn restore(app: &App) {
    let bytes = match std::fs::read(queue_file(app.paths())) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::debug!(%err, "персиста очереди нет — стартуем с пустой");
            return;
        }
    };
    let state: PersistState = match serde_json::from_slice(&bytes) {
        Ok(state) => state,
        Err(err) => {
            tracing::debug!(%err, "персист очереди не читается — стартуем с пустой");
            return;
        }
    };
    let len = state.queue.len();
    app.player()
        .with_queue(|q| q.restore(state.queue, state.index, state.loop_mode, state.shuffle))
        .await;
    tracing::info!("очередь восстановлена: {} треков", len);
}
