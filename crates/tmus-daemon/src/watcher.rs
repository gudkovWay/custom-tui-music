use std::sync::Arc;
use std::time::Duration;

use tmus_core::model::{PlaybackStatus, TrackId};
use tmus_core::protocol::Event;

use crate::app::App;

/// Вахтер состояния: рассылает позицию и замечает смену трека.
///
/// Почему не подписка на события mpv: `Player` забирает приёмник себе в
/// конструкторе и сам ведёт переходы по очереди — второго читателя у
/// `mpsc` быть не может. Поэтому вахтёр просыпается по двум причинам:
/// раз в секунду (позиция) и по сигналу `Player::changed`, который
/// плеер даёт на смену трека, паузу, приехавшую длительность и падение
/// mpv. Чистый секундный опрос давал замеренный рассинк бара: до
/// секунды `--:--` вместо длительности и двойная перерисовка на скип.
///
/// Здесь же единственное место, где состояние сравнивается с прошлым:
/// подписчик получает `TrackChanged` ровно один раз на трек, а не на
/// каждый тик — поэтому команды сами событий смены трека не шлют.
pub async fn run_state_watcher(app: Arc<App>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut last_track: Option<TrackId> = None;
    let mut last_status = PlaybackStatus::Stopped;
    let mut last_duration: Option<Duration> = None;
    let mut last_queue = (0usize, None);

    loop {
        // Тик нужен позиции, сигнал — всему остальному: первый из двух
        // и будит цикл.
        let ticked = tokio::select! {
            _ = ticker.tick() => true,
            () = app.player.changed() => false,
        };
        let state = app.player.state().await;

        // Радио-догрузка: переход плеера (смена трека, шаффл, конец
        // трека) мог придвинуть очередь к хвосту — даём сессии шанс
        // дописать рекомендации. Внутри — дешёвая проверка под замком;
        // сетевой заход уходит в фоновую задачу.
        app.maybe_refill_radio().await;

        let track = state.track.as_ref().map(|t| t.id.clone());
        if track != last_track {
            last_track = track;
            app.emit(Event::TrackChanged {
                track: state.track.clone(),
                queue_index: state.queue_index,
            });
            app.emit(Event::StateChanged { state: state.clone() });
        } else if state.status != last_status || state.duration != last_duration {
            // Длительность приезжает от mpv позже загрузки, и без её
            // рассылки бар до следующей смены трека рисовал бы `--:--`.
            app.emit(Event::StateChanged { state: state.clone() });
        }
        last_status = state.status;
        last_duration = state.duration;

        let queue = (state.queue_len, state.queue_index);
        if queue != last_queue {
            last_queue = queue;
            app.emit(Event::QueueChanged { len: queue.0, index: queue.1 });
        }

        if !ticked || !matches!(state.status, PlaybackStatus::Playing) {
            continue;
        }
        if let Some(position) = state.position {
            app.emit(Event::Position { position, duration: state.duration });
        }
    }
}
