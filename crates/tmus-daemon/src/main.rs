//! `tmusd` — демон. Держит mpv, очередь, офлайн-кэш и четыре
//! потребителя: control-socket, MPRIS, иконку в трее и Discord RPC.
//!
//! Почему демон отдельно от интерфейса: музыка обязана играть с закрытым
//! терминалом. Это же закрывает требование «не закрывался, а
//! сворачивался в трей» — закрытие TUI ничего не останавливает, а
//! иконка SNI остаётся.
//!
//! Провайдеры регистрируются здесь и только здесь. Ни одна подсистема
//! ниже не знает, какой сервис подключён.

mod app;
mod cache_layer;
mod catalog;
mod control;
mod filler;
mod mpris;
mod orphan_sweep;
mod persist;
mod rpc;
mod singleton;
mod tray;
mod watcher;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use tmus_core::cache::Cache;
use tmus_core::config::Config;
use tmus_core::paths::Paths;
use tmus_player::Player;
use tmus_provider::Registry;

use crate::app::App;

#[derive(clap::Parser)]
#[command(name = "tmusd", about = "Демон терминального музыкального плеера")]
struct Args {
    /// Не поднимать иконку в трее.
    #[arg(long)]
    no_tray: bool,
    /// Не подключаться к Discord.
    #[arg(long)]
    no_rpc: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("TMUS_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = <Args as clap::Parser>::parse();

    let paths = Paths::resolve().context("не разрешились пути XDG")?;
    paths.ensure_dirs().context("не создались каталоги")?;

    // Эксклюзивный запуск: замок берём ДО sweep_orphan_mpv и mpv —
    // иначе дубликат успел бы подметать чужих mpv (гробил mpv живого
    // демона, 21.09) и поднимать собственного. Ждём ~10 с: рестарт
    // юнита гасит старый демон примерно столько. Держим до конца main —
    // release случится сам при закрытии дескриптора на выходе процесса.
    // При занятом замке выходим с кодом 0: второй демон не должен
    // красить systemd-юнит и не должен трогать файл очереди.
    let _singleton = match singleton::acquire(&paths.lock_file(), Duration::from_secs(10))
        .context("замок демона не открылся")?
    {
        Some(lock) => lock,
        None => {
            tracing::error!(
                "демон уже работает ({}): второй не стартую",
                paths.lock_file().display()
            );
            return Ok(());
        }
    };

    let config = Config::load(&paths).context("конфиг не прочитался")?;
    let cache = Arc::new(std::sync::Mutex::new(
        Cache::open(&paths, config.cache.limit_bytes).context("кэш не открылся")?,
    ));

    // Подметаем осиротевшие mpv ДО поднятия своего плеера: инцидент
    // 19.09 — после гибели демона без штатного завершения (kill -9, OOM)
    // накопилось 72 осиротевших mpv на ~3.4 ГБ RSS. KillMode=control-group
    // тут не спасает: он работает, только когда юнит гасит сам свой cgroup,
    // а при жёсткой смерти демон не успевает никого убить — убирать должно
    // следующее включение. Свои дети (запущенные уже после этой точки)
    // живут в нашем cgroup и подметальщику не трогают их. Сокет передаём
    // свой: подметаются только остатки НАШЕГО рантайма, чужой
    // (изолированный стенд) неприкосновенен — см. `orphan_sweep`.
    orphan_sweep::sweep_orphan_mpv(&paths.mpv_socket());

    // Резолв каждого провайдера оборачивается прослойкой кэша: есть
    // файл на диске — в сеть не идём вовсе. Иначе кэш был бы
    // односторонним, и заказанный офлайн не работал бы.
    let registry = build_registry(&config, &cache)?;
    if registry.is_empty() {
        // Пустой реестр — не отказ: демон поднимается и играет из
        // офлайн-кэша. Но сказать об этом надо, иначе человек будет
        // искать причину в плеере.
        tracing::warn!("ни один провайдер не подключён — доступен только офлайн-кэш");
    }

    // Семафор резолвов — один на процесс: он сериализует yt-dlp и у
    // плеера, и у филлера. Создаётся здесь ровно один раз и
    // внедряется в оба, иначе плеер и филлер не знали бы друг о друге
    // и запускали по параллельному yt-dlp (~0.9 CPU-с и 335 МБ каждый).
    let resolve_gate = Arc::new(tokio::sync::Semaphore::new(1));

    let player = Player::new(
        registry.clone(),
        config.mpv.clone(),
        &paths,
        config.volume_clamped(),
        Arc::clone(&resolve_gate),
    )
        .await
        .context("mpv не поднялся")?;

    let app = App::new(player.clone(), registry, cache, config, paths, resolve_gate);

    // Конфиговый EQ — стартовое значение ДО персиста: если queue.json
    // несёт сохранённый эквалайзер, restore перезапишет им; если нет
    // (первый запуск, старый файл), действует настройка из конфига.
    // Громкость из конфига уже внутри Player::new — здесь не дублируется.
    if let Err(err) = app.player().set_equalizer(app.config().equalizer.clone()).await {
        tracing::warn!(%err, "конфиговый эквалайзер не применился");
    }

    // Очередь восстанавливаем ДО поднятия подсистем: watcher и MPRIS
    // должны увидеть уже восстановленное состояние, а не пустую очередь,
    // которая «вдруг» сменится на заполненную через миллисекунды.
    persist::restore(&app).await;

    let mut tasks = tokio::task::JoinSet::new();

    // Переходы по очереди ведёт сам `Player` (он забрал приёмник событий
    // mpv в своём конструкторе), поэтому демону нужен не второй цикл, а
    // вахтер: он замечает смену трека опросом и рассылает её
    // подписчикам.
    tasks.spawn(watcher::run_state_watcher(app.clone()));
    tasks.spawn(persist::run(app.clone()));
    if app.config().cache.prefetch_next {
        tasks.spawn(filler::run_cache_filler(app.clone()));
    }

    // Control-socket — единственная обязательная подсистема: без него
    // демоном нельзя управлять вообще.
    {
        let app = app.clone();
        tasks.spawn(async move {
            if let Err(err) = control::run(app).await {
                tracing::error!(%err, "control-socket остановился");
            }
        });
    }

    // Остальные три деградируют молча: MPRIS без сессионной шины, трей
    // без хоста SNI, RPC без запущенного Discord — штатные состояния, а
    // не причина не играть музыку.
    {
        let app = app.clone();
        tasks.spawn(async move {
            if let Err(err) = mpris::run(app).await {
                tracing::warn!(%err, "MPRIS недоступен");
            }
        });
    }
    if !args.no_tray {
        let app = app.clone();
        tasks.spawn(async move {
            if let Err(err) = tray::run(app).await {
                tracing::warn!(%err, "иконка в трее недоступна");
            }
        });
    }
    if !args.no_rpc && app.config().discord_rpc {
        let app = app.clone();
        tasks.spawn(async move {
            if let Err(err) = rpc::run(app).await {
                tracing::warn!(%err, "Discord RPC недоступен");
            }
        });
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("получен SIGINT, гашу демон"),
        _ = terminate() => tracing::info!("получен SIGTERM, гашу демон"),
        _ = app.wait_shutdown() => tracing::info!("гашусь по команде клиента"),
    }

    // Порядок важен. Сначала снимаем задачи, чтобы никто не отправил в
    // mpv новую команду после `quit`. Потом гасим mpv: он подпроцесс, и
    // без этого переживает демон — замерено, три прогона стенда оставили
    // пять осиротевших mpv по ~100 МБ. И только потом убираем файл
    // сокета, иначе следующий запуск увидит мёртвый сокет и откажется
    // стартовать.
    tasks.shutdown().await;
    // Финальный сброс очереди: задачи уже сняты, их грязный тик не
    // случится — пишем сами. Ошибка не критична, но молчать о ней нельзя:
    // человек потеряет очередь и не поймёт почему.
    if let Err(err) = persist::flush_now(&app).await {
        tracing::warn!(%err, "финальный сброс очереди не удался");
    }
    player.mpv().shutdown().await;
    let _ = std::fs::remove_file(app.paths().control_socket());
    Ok(())
}

/// Провайдеры, известные сборке. Единственное место, где крейт
/// провайдера вообще упоминается.
fn build_registry(
    config: &Config,
    cache: &Arc<std::sync::Mutex<tmus_core::cache::Cache>>,
) -> anyhow::Result<Registry> {
    let mut registry = Registry::new();

    if config.provider(tmus_core::ProviderId::YTMUSIC.as_str()).enabled {
        match tmus_core::cookies::source_from_config(config) {
            Ok(cookies) => match tmus_ytmusic::YtMusic::new(config, cookies) {
                Ok(provider) => {
                    registry.insert(crate::cache_layer::Cached::wrap(
                        Arc::new(provider),
                        Arc::clone(cache),
                    ));
                }
                // Отсутствие сессии — не повод не стартовать: офлайн-кэш
                // работает и без неё, а причину человек увидит в баре.
                Err(err) => tracing::warn!(%err, "YouTube Music не подключился"),
            },
            Err(err) => tracing::warn!(%err, "cookies браузера не нашлись"),
        }
    }

    Ok(registry)
}


#[cfg(unix)]
async fn terminate() {
    if let Ok(mut signal) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        signal.recv().await;
    }
}
