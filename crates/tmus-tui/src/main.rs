//! `tmus` — клиент демона. Без аргументов поднимает TUI, с подкомандой
//! делает один запрос и печатает результат; подкоманды вешаются на
//! медиа-клавиши Hyprland и дергаются плагином noctalia, поэтому каждая
//! обязана завершаться сама и не трогать терминал.

mod client;
mod ui;

use std::time::Duration;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tmus_core::model::{EqState, LoopMode, PlaylistId, ProviderId, Rating, SearchKind, TrackId, EQ_GAIN_LIMIT_DB};
use tmus_core::protocol::{Cmd, Payload};
use tmus_core::Paths;

#[derive(Parser)]
#[command(name = "tmus", about = "Клиент tmus: TUI и одноразовые команды")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<CliCmd>,
}

#[derive(Subcommand)]
enum CliCmd {
    /// Диагностика: RTT демона без поднятия нового.
    Ping,
    /// Играть трек (по умолчанию — текущий/первый из очереди).
    Play { track_id: Option<String> },
    Pause,
    Toggle,
    Stop,
    Next,
    Prev,
    /// Позиция в секундах: со знаком — относительный сдвиг; минус —
    /// значение, не флаг (см. Vol).
    Seek {
        #[arg(allow_hyphen_values = true)]
        seconds: String,
    },
    /// Громкость 0..100: со знаком — от текущей. Минус обязан
    /// разбираться как значение, а не флаг: панель шлёт `vol -5`.
    Vol {
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Эквалайзер: без флагов печатает состояние, с флагами правит.
    /// Флаги комбинируются, каждый заданный — применяется; один
    /// `--band` за вызов.
    Eq {
        #[arg(long)]
        on: bool,
        #[arg(long)]
        off: bool,
        #[arg(long)]
        preset: Option<String>,
        /// Полоса и абсолютное усиление: `3:-4.5` — третья полоса
        /// в −4.5 дБ. Абсолютное, а не относительное: панель noctalia
        /// считает дельту у себя и шлёт итог.
        #[arg(long)]
        band: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Состояние плеера.
    Status {
        #[arg(long)]
        json: bool,
    },
    Providers {
        #[arg(long)]
        json: bool,
    },
    Liked {
        #[arg(long)]
        json: bool,
    },
    /// Оценить трек: `tmus rate <provider>:<id> like|dislike|none`.
    Rate { track: String, rating: String },
    /// Все локальные оценки демона.
    Ratings {
        #[arg(long)]
        json: bool,
    },
    Source {
        arg: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Library {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Треки плейлиста.
    LibraryTracks {
        playlist_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Домашняя лента рекомендаций (полки).
    Home {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Перейти к треку очереди по индексу (0-базный) и играть.
    QueueGoto { index: usize },
    Loop {
        mode: String,
    },
    Shuffle {
        mode: String,
    },
    Queue {
        #[arg(long)]
        json: bool,
    },
    /// Поиск; `--kind` выбирает, что искать.
    Search {
        query: String,
        /// `all` — треки, артисты и плейлисты тремя запросами одним
        /// списком; без флага ищем треки, как раньше.
        #[arg(long, value_enum, default_value_t = SearchKindArg::Tracks)]
        kind: SearchKindArg,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Играть плейлист с начала или с `--start`.
    PlayPlaylist {
        playlist_id: String,
        #[arg(long)]
        start: Option<usize>,
    },
    /// Работа с плейлистами. `pl`, а не `playlist`: подкоманду дергает
    /// сервис плагина noctalia на каждое действие пользователя с
    /// плейлистами, и сокращение экономит и строку конфига, и набор в
    /// шелле.
    #[command(subcommand)]
    Pl(PlCmd),
    #[command(subcommand)]
    Cache(CacheCmd),
    /// Стрим событий демона построчно в stdout (для noctalia.runStream).
    Events,
    StopDaemon,
}

#[derive(Subcommand)]
enum CacheCmd {
    Stats {
        #[arg(long)]
        json: bool,
    },
    Pin { track_id: String },
    Unpin { track_id: String },
    Gc,
    /// Докачать плейлист в офлайн-кэш в фоне.
    Warm { playlist_id: String },
}

/// Подкоманды `tmus pl`. Названия повторяют контракт сервиса плагина:
/// `new/add/rm/del/ls` зовутся из luau-кода ровно в такой форме.
#[derive(Subcommand)]
enum PlCmd {
    /// Создать плейлист; печатает id созданного.
    New { title: String },
    /// Добавить трек в плейлист.
    Add { playlist_id: String, track: String },
    /// Убрать трек из плейлиста.
    Rm { playlist_id: String, track: String },
    /// Удалить плейлист вместе с содержимым.
    Del { playlist_id: String },
    /// Список библиотечных плейлистов; `--json` — как есть из демона.
    Ls {
        #[arg(long)]
        json: bool,
    },
}

/// Метка жизни потока `events`: печатается при простое, парсится
/// сервисом плагина как неизвестное событие и игнорируется всем
/// остальным. Отдельная const, а не литерал в `println!`: у форм-строки
/// с одним строковым литералом clippy справедливо спрашивает, зачем
/// формат вообще, а литерал с `{}` в `println!` — это сломанный формат.
const KEEPALIVE_LINE: &str = r#"{"event":"keepalive"}"#;

/// `--kind` команды `search`. Отдельный тип, а не свободная строка:
/// набор значений задаёт clap, поэтому опечатка падает до похода в
/// демон, а не превращается в пустой результат.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum SearchKindArg {
    /// Треки, артисты и плейлисты: три запроса, один список.
    All,
    Tracks,
    Artists,
    Playlists,
}

impl SearchKindArg {
    /// Виды протокола, которые надо спросить, в порядке склейки.
    #[must_use]
    fn kinds(self) -> &'static [SearchKind] {
        match self {
            Self::All => &[SearchKind::Tracks, SearchKind::Artists, SearchKind::Playlists],
            Self::Tracks => &[SearchKind::Tracks],
            Self::Artists => &[SearchKind::Artists],
            Self::Playlists => &[SearchKind::Playlists],
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::resolve()?;

    let Some(cmd) = cli.cmd else {
        return ui::run(&paths).await;
    };

    // `events` — не команда демона, а режим подписки: своё соединение,
    // из которого читаем бесконечно. По этой же причине идёт до
    // connect_or_spawn — демон при необходимости поднимет подписка.
    if let CliCmd::Events = cmd {
        return run_events(&paths).await;
    }

    if let CliCmd::Ping = cmd {
        return run_ping(&paths).await;
    }

    let mut client = client::Client::connect_or_spawn(&paths).await?;
    let cmd = match cmd {
        CliCmd::Events => unreachable!("обработан выше"),
        CliCmd::Play { track_id } => match track_id {
            Some(id) => Cmd::PlayTrack { track: parse_track_id(&id)? },
            // Без id просим играть то, что уже выбрано: медиа-клавиша
            // Hyprland не знает про очередь, это знает демон.
            None => Cmd::Play,
        },
        CliCmd::Pause => Cmd::Pause,
        CliCmd::Toggle => Cmd::Toggle,
        CliCmd::Stop => Cmd::Stop,
        CliCmd::Next => Cmd::Next,
        CliCmd::Prev => Cmd::Prev,
        CliCmd::Seek { seconds } => parse_seek(&seconds)?,
        CliCmd::Vol { value } => parse_vol(&value, current_volume(&mut client).await?)?,
        CliCmd::Eq { on, off, preset, band, json } => {
            let enabled = match (on, off) {
                (true, true) => bail!("--on и --off вместе задавать нельзя"),
                (true, false) => Some(true),
                (false, true) => Some(false),
                (false, false) => None,
            };
            // `--band` задаёт одну полосу абсолютным значением, а демон
            // принимает только полную десятку: текущие полосы читаем
            // здесь и подменяем один элемент.
            let bands = match &band {
                Some(spec) => {
                    let (index, gain) = parse_band(spec)?;
                    let mut eq = current_equalizer(&mut client).await?;
                    eq[index] = gain;
                    Some(eq.to_vec())
                }
                None => None,
            };
            if enabled.is_none() && preset.is_none() && bands.is_none() {
                let payload = client.call(Cmd::State).await?;
                if json {
                    return print_payload(payload, true);
                }
                match payload {
                    Payload::State(state) => println!("{}", format_eq(&state.equalizer)),
                    _ => bail!("неожиданный ответ на State"),
                }
                return Ok(());
            }
            Cmd::Equalizer { enabled, preset, bands }
        }
        CliCmd::Status { json } => return print_payload(client.call(Cmd::State).await?, json),
        CliCmd::Providers { json } => return print_payload(client.call(Cmd::Providers).await?, json),
        CliCmd::Library { provider, json } => {
            let provider = resolve_provider(&mut client, provider.as_deref()).await?;
            return print_payload(client.call(Cmd::Library { provider }).await?, json);
        }
        CliCmd::Home { provider, json } => {
            let provider = resolve_provider(&mut client, provider.as_deref()).await?;
            return print_payload(client.call(Cmd::Home { provider }).await?, json);
        }
        CliCmd::Liked { json } => return print_payload(client.call(Cmd::Liked { provider: None }).await?, json),
        CliCmd::Rate { track, rating } => Cmd::Rate {
            track: parse_track_id(&track)?,
            rating: parse_rating(&rating)?,
        },
        CliCmd::Ratings { json } => return print_payload(client.call(Cmd::Ratings).await?, json),
        CliCmd::Source { arg, json } => {
            let cmd = match arg {
                Some(arg) => Cmd::SetCatalogSource {
                    source: tmus_core::protocol::CatalogSource { provider: parse_source_arg(&arg)? },
                },
                None => Cmd::GetCatalogSource,
            };
            return print_payload(client.call(cmd).await?, json);
        }
        CliCmd::LibraryTracks { playlist_id, json } => {
            let cmd = Cmd::LibraryTracks { playlist: parse_playlist_id(&playlist_id)? };
            return print_payload(client.call(cmd).await?, json);
        }
        CliCmd::Loop { mode } => Cmd::SetLoop { mode: parse_loop(&mode)? },
        CliCmd::Shuffle { mode } => match parse_shuffle(&mode)? {
            Some(shuffle) => Cmd::SetShuffle { shuffle },
            None => {
                let current = match client.call(Cmd::State).await? {
                    Payload::State(state) => state.shuffle,
                    _ => false,
                };
                Cmd::SetShuffle { shuffle: !current }
            }
        },
        CliCmd::Queue { json } => return print_payload(client.call(Cmd::Queue).await?, json),
        CliCmd::QueueGoto { index } => {
            return print_payload(client.call(Cmd::QueueGoto { index }).await?, false)
        }
        CliCmd::Search { query, kind, provider, json } => {
            let provider = resolve_provider(&mut client, provider.as_deref()).await?;
            return run_search(&mut client, &query, kind, provider, json).await;
        }
        CliCmd::PlayPlaylist { playlist_id, start } => Cmd::PlayPlaylist {
            playlist: parse_playlist_id(&playlist_id)?,
            start: Some(start.unwrap_or(0)),
        },
        CliCmd::Pl(action) => return run_pl(&mut client, action).await,
        CliCmd::Cache(cache) => match cache {
            CacheCmd::Stats { json } => return print_payload(client.call(Cmd::CacheStats).await?, json),
            CacheCmd::Pin { track_id } => {
                Cmd::CachePin { tracks: vec![parse_track_id(&track_id)?] }
            }
            CacheCmd::Unpin { track_id } => {
                Cmd::CacheUnpin { tracks: vec![parse_track_id(&track_id)?] }
            }
            CacheCmd::Gc => Cmd::CacheGc,
            CacheCmd::Warm { playlist_id } => {
                // Двухходовка: сначала треки плейлиста, затем заказ
                // фонового прогрева. Ошибка любого хода — Err в caller,
                // как у остальных подкоманд.
                let ids = match client
                    .call(Cmd::LibraryTracks { playlist: parse_playlist_id(&playlist_id)? })
                    .await?
                {
                    Payload::Tracks(tracks) => tracks.into_iter().map(|t| t.id).collect::<Vec<_>>(),
                    _ => anyhow::bail!("неожиданный ответ на LibraryTracks"),
                };
                let n = ids.len();
                match client.call(Cmd::CacheWarm { tracks: ids }).await? {
                    Payload::Ack(_) => {
                        println!("грею {} треков в фоне; прогресс — tmus cache stats", n);
                    }
                    _ => anyhow::bail!("неожиданный ответ на CacheWarm"),
                }
                return Ok(());
            }
        },
        CliCmd::StopDaemon => Cmd::Shutdown,
        // Обработан до connect_or_spawn; ветка для полноты match.
        CliCmd::Ping => unreachable!("обработан выше"),
    };

    print_payload(client.call(cmd).await?, false)
}

/// `tmus pl …`: пять действий над плейлистами. Все ходят в те же Cmd,
/// что и TUI; `new` разворачивает `Payload::PlaylistCreated` в голый
/// id — потребителю (сервису плагина) больше ничего не нужно, а id он
/// сразу передаёт в `pl add`. `ls` переиспользует путь `library/list`
/// (`Cmd::Library`): отдельной команды плейлистов в протоколе нет.
async fn run_pl(client: &mut client::Client, action: PlCmd) -> Result<()> {
    match action {
        PlCmd::New { title } => match client.call(Cmd::PlaylistCreate { title }).await? {
            Payload::PlaylistCreated { playlist } => {
                println!("{playlist}");
                Ok(())
            }
            _ => bail!("неожиданный ответ на PlaylistCreate"),
        },
        PlCmd::Add { playlist_id, track } => {
            let cmd = Cmd::PlaylistAdd {
                playlist: parse_playlist_id(&playlist_id)?,
                track: parse_track_id(&track)?,
            };
            print_payload(client.call(cmd).await?, false)
        }
        PlCmd::Rm { playlist_id, track } => {
            let cmd = Cmd::PlaylistRemove {
                playlist: parse_playlist_id(&playlist_id)?,
                track: parse_track_id(&track)?,
            };
            print_payload(client.call(cmd).await?, false)
        }
        PlCmd::Del { playlist_id } => {
            let cmd = Cmd::PlaylistDelete { playlist: parse_playlist_id(&playlist_id)? };
            print_payload(client.call(cmd).await?, false)
        }
        PlCmd::Ls { json } => {
            let provider = resolve_provider(client, None).await?;
            print_payload(client.call(Cmd::Library { provider }).await?, json)
        }
    }
}

/// `tmus ping`: время отклика живого демона (connect + Cmd::State).
/// В отличие от остальных подкоманд НЕ автозапускает демона — смысл
/// команды в диагностике того, что уже работает; поднятый ради пинга
/// новый демон исказил бы результат. Состояние не трогает.
async fn run_ping(paths: &Paths) -> Result<()> {
    let t0 = std::time::Instant::now();
    let result = async {
        let mut client = client::Client::connect(paths).await?;
        client.call(Cmd::State).await
    }
    .await;
    match result {
        Ok(_) => {
            println!("tmusd отвечает за {} мс", t0.elapsed().as_millis());
            Ok(())
        }
        Err(e) => {
            eprintln!("tmusd не отвечает: {e}");
            Err(e)
        }
    }
}

/// Текущая громкость нужна только для относительной `vol`; отдельный
/// запрос дешевле и честнее, чем хранить состояние в клиенте, который
/// живёт полсекунды.
async fn current_volume(client: &mut client::Client) -> Result<f64> {
    Ok(match client.call(Cmd::State).await? {
        Payload::State(state) => state.volume,
        _ => 0.0,
    })
}

/// Текущие полосы нужны `--band`: демон принимает только полную
/// десятку, а клиент правит одну полосу.
async fn current_equalizer(client: &mut client::Client) -> Result<[f64; 10]> {
    Ok(match client.call(Cmd::State).await? {
        Payload::State(state) => state.equalizer.bands,
        _ => [0.0; 10],
    })
}

/// Короткий текст состояния эквалайзера: `eq off` либо
/// `eq on <preset> +5.0 +4.0 …` — все десять полос с одной десятичной
/// и ведущим знаком.
fn format_eq(eq: &EqState) -> String {
    if !eq.enabled {
        return "eq off".to_owned();
    }
    let bands = eq
        .bands
        .iter()
        .map(|g| format!("{g:+.1}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("eq on {} {bands}", eq.preset)
}

/// `"3:-4.5" → (2, -4.5)`. Полоса 1-базная (1..=10), усиление в дБ
/// −[`EQ_GAIN_LIMIT_DB`]..=[`EQ_GAIN_LIMIT_DB`]. Чистая функция ради
/// тестов: разбор аргумента не должен требовать живого демона.
fn parse_band(s: &str) -> Result<(usize, f64)> {
    let Some((raw_n, raw_gain)) = s.split_once(':') else {
        bail!("полоса: ожидается N:GAIN, например 3:-4.5, получено {s:?}")
    };
    let n: usize = raw_n
        .parse()
        .map_err(|_| anyhow::anyhow!("номер полосы: целое 1..=10, получено {raw_n:?}"))?;
    if !(1..=10).contains(&n) {
        bail!("номер полосы: 1..=10, получено {n}")
    }
    let gain: f64 = raw_gain
        .parse()
        .map_err(|_| anyhow::anyhow!("усиление: число в дБ, получено {raw_gain:?}"))?;
    if !(-EQ_GAIN_LIMIT_DB..=EQ_GAIN_LIMIT_DB).contains(&gain) {
        bail!("усиление: -{EQ_GAIN_LIMIT_DB}..={EQ_GAIN_LIMIT_DB} дБ, получено {gain}")
    }
    Ok((n - 1, gain))
}

fn print_payload(payload: Payload, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(&payload)?);
        return Ok(());
    }
    println!("{}", format_payload(&payload));
    Ok(())
}

/// Короткий текст для человека. Отдельная функция, чтобы `--json`
/// оставался точным зеркалом протокола.
fn format_payload(payload: &Payload) -> String {
    match payload {
        Payload::Ack(_) => "ok".to_owned(),
        Payload::State(s) => {
            let track = s
                .track
                .as_ref()
                .map(|t| format!("{} — {}", t.artist_line(), t.title))
                .unwrap_or_else(|| "—".to_owned());
            let pos = fmt_time(s.position.map(|d| d.as_secs()));
            let dur = fmt_time(s.duration.map(|d| d.as_secs()));
            format!("{track} [{:?}] {pos}/{dur} vol={}{}", s.status, s.volume.round() as u64, if s.shuffle { " shuffle" } else { "" })
        }
        Payload::Queue(q) => q
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{}: {} — {}", i, t.artist_line(), t.title))
            .collect::<Vec<_>>()
            .join("\n"),
        Payload::Tracks(tracks) => tracks
            .iter()
            .map(|t| format!("{}: {} — {}", t.id, t.artist_line(), t.title))
            .collect::<Vec<_>>()
            .join("\n"),
        Payload::Results(results) => results
            .iter()
            .map(format_search_result)
            .collect::<Vec<_>>()
            .join("\n"),
        Payload::Playlists(playlists) => playlists
            .iter()
            .map(|p| format!("{}: {}", p.id, p.title))
            .collect::<Vec<_>>()
            .join("\n"),
        Payload::Home(shelves) => shelves
            .iter()
            .map(|shelf| {
                let head = match &shelf.subtitle {
                    Some(s) => format!("▌{} — {}", shelf.title, s),
                    None => format!("▌{}", shelf.title),
                };
                let items =
                    shelf.items.iter().map(format_search_result).collect::<Vec<_>>().join("\n  ");
                format!("{head}\n  {items}")
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        Payload::Providers(providers) => providers
            .iter()
            .map(|p| format!("{} ({}): {:?}", p.id, p.name, p.auth))
            .collect::<Vec<_>>()
            .join("\n"),
        Payload::Catalog(s) => s.provider.clone().unwrap_or_else(|| "all".to_owned()),
        Payload::Cache(c) => format!(
            "треков {} ({:.1} МиБ), закреплено {} ({:.1} МиБ), лимит {:.1} МиБ",
            c.tracks,
            c.bytes as f64 / 1048576.0,
            c.pinned_tracks,
            c.pinned_bytes as f64 / 1048576.0,
            c.limit_bytes as f64 / 1048576.0,
        ),
        Payload::Ratings(ratings) => ratings
            .iter()
            .map(|(id, r)| format!("{id}: {r:?}"))
            .collect::<Vec<_>>()
            .join("\n"),
        // Содержательный вид нужен только CLI `pl new`, который
        // разворачивает payload сам; здесь — просто id.
        Payload::PlaylistCreated { playlist } => playlist.to_string(),
    }
}

/// Формат одного элемента полки/выдачи: общий у `Results` и полок `Home`,
/// чтобы лента выглядела как продолжение поиска.
fn format_search_result(r: &tmus_core::model::SearchResult) -> String {
    match r {
        tmus_core::model::SearchResult::Track(t) => {
            format!("{}: {} — {}", t.id, t.artist_line(), t.title)
        }
        tmus_core::model::SearchResult::Playlist(p) => format!("{}: {}", p.id, p.title),
        tmus_core::model::SearchResult::Artist { provider, id, name } => {
            format!("{provider}:{id}: {name}")
        }
    }
}

async fn run_events(paths: &Paths) -> Result<()> {
    let mut rx = client::Client::subscribe(paths).await?;
    loop {
        // По одному JSON на строку с flush: потребитель — runStream
        // плагина, без flush бар обновляется рывками по буферу.
        //
        // Keepalive при простое: у noctalia.runStream нет колбэка
        // смерти потока, и сервис плагина отличает «демон молчит,
        // потому что на паузе» от «поток умер вместе с демоном» только
        // по молчанию. На паузе демон событий не шлёт вовсе, поэтому
        // клиент сам отмечается каждые 2 с простоя.
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };
                println!("{}", serde_json::to_string(&event)?);
                use std::io::Write as _;
                std::io::stdout().flush()?;
            }
            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                println!("{}", KEEPALIVE_LINE);
                use std::io::Write as _;
                std::io::stdout().flush()?;
            }
        }
    }
    Ok(())
}

/// `"<provider>:<id>"`. Деление по ПЕРВОМУ двоеточию: id у провайдеров
/// бывает с двоеточием внутри (`video:abc`).
pub fn parse_track_id(s: &str) -> Result<TrackId> {
    let (provider, id) = split_provider(s)?;
    Ok(TrackId::new(provider, id))
}

pub fn parse_playlist_id(s: &str) -> Result<PlaylistId> {
    let (provider, id) = split_provider(s)?;
    Ok(PlaylistId::new(provider, id))
}

fn split_provider(s: &str) -> Result<(ProviderId, String)> {
    let Some((name, id)) = s.split_once(':') else {
        bail!("ожидался идентификатор вида <provider>:<id>, получено {s:?}");
    };
    let provider = ProviderId::from_name(name).ok_or_else(|| {
        let known = ProviderId::ALL.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(", ");
        anyhow::anyhow!("неизвестный провайдер {name:?}; известны: {known}")
    })?;
    Ok((provider, id.to_owned()))
}

/// Знак решает режим: `"+15"`/`"-15"` — сдвиг от текущей позиции,
/// `"90"` — абсолютная позиция от начала.
pub fn parse_seek(s: &str) -> Result<Cmd> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('+') {
        return Ok(Cmd::SeekBy { delta: rest.parse()? });
    }
    if let Some(rest) = s.strip_prefix('-') {
        return Ok(Cmd::SeekBy { delta: -rest.parse::<f64>()? });
    }
    Ok(Cmd::Seek { position: Duration::from_secs_f64(s.parse()?) })
}

/// `"+5"`/`"-5"` считаются от текущей и зажимаются в 0..100, `"40"` —
/// абсолютное значение.
pub fn parse_vol(s: &str, current: f64) -> Result<Cmd> {
    let s = s.trim();
    let value = if let Some(rest) = s.strip_prefix('+') {
        (current + rest.parse::<f64>()?).clamp(0.0, 100.0)
    } else if let Some(rest) = s.strip_prefix('-') {
        (current - rest.parse::<f64>()?).clamp(0.0, 100.0)
    } else {
        s.parse::<f64>()?.clamp(0.0, 100.0)
    };
    Ok(Cmd::SetVolume { volume: value })
}

fn parse_source_arg(s: &str) -> Result<Option<String>> {
    if s == "all" {
        Ok(None)
    } else {
        Ok(Some(s.to_owned()))
    }
}

fn parse_provider_flag(s: &str) -> Option<String> {
    (s != "all").then(|| s.to_owned())
}

async fn resolve_provider(
    client: &mut client::Client,
    flag: Option<&str>,
) -> Result<Option<String>> {
    match flag {
        Some(flag) => Ok(parse_provider_flag(flag)),
        None => Ok(match client.call(Cmd::GetCatalogSource).await? {
            Payload::Catalog(source) => source.provider,
            _ => None,
        }),
    }
}

/// Один вид — один запрос, ответ демона печатаем как есть: форма payload
/// не должна зависеть от того, задан `--kind` явно или нет. `all` —
/// столько запросов, сколько видов, с тем же провайдером и запросом;
/// склейка здесь, а не в демоне: демон ищет строго в рамках вида, и
/// расширять протокол ради сахара незачем.
async fn run_search(
    client: &mut client::Client,
    query: &str,
    kind: SearchKindArg,
    provider: Option<String>,
    json: bool,
) -> Result<()> {
    let kinds = kind.kinds();
    if let [only] = kinds {
        let payload = client
            .call(Cmd::Search { query: query.to_owned(), kind: *only, provider })
            .await?;
        return print_payload(payload, json);
    }

    let mut results = Vec::new();
    for search_kind in kinds {
        let cmd = Cmd::Search {
            query: query.to_owned(),
            kind: *search_kind,
            provider: provider.clone(),
        };
        match client.call(cmd).await? {
            Payload::Results(mut part) => results.append(&mut part),
            // Молча ронять часть выдачи нельзя: список выглядел бы
            // полным, не будучи им.
            other => bail!("поиск {search_kind:?} вернул не результаты: {other:?}"),
        }
    }
    print_payload(Payload::Results(results), json)
}

fn parse_loop(s: &str) -> Result<LoopMode> {
    match s {
        "none" => Ok(LoopMode::None),
        "track" => Ok(LoopMode::Track),
        "queue" => Ok(LoopMode::Queue),
        _ => bail!("режим повтора: none|track|queue, получено {s:?}"),
    }
}

/// Слово оценки команды `rate`. Отдельная функция ради тестов: разбор
/// аргумента не должен требовать живого демона.
fn parse_rating(s: &str) -> Result<Rating> {
    match s {
        "like" => Ok(Rating::Liked),
        "dislike" => Ok(Rating::Disliked),
        "none" => Ok(Rating::None),
        _ => bail!("оценка: like|dislike|none, получено {s:?}"),
    }
}

fn parse_shuffle(s: &str) -> Result<Option<bool>> {
    match s {
        "on" => Ok(Some(true)),
        "off" => Ok(Some(false)),
        "toggle" => Ok(None),
        _ => bail!("режим шафла: on|off|toggle, получено {s:?}"),
    }
}

/// `187 с → "3:07"`, `3723 с → "1:02:03"`. `None` — длительность
/// неизвестна провайдеру.
pub fn fmt_time(secs: Option<u64>) -> String {
    let Some(secs) = secs else { return "--:--".to_owned() };
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        // Минуты без ведущего нуля: acceptance задаёт «3:07».
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_id_splits_at_first_colon() {
        let id = parse_track_id("ytmusic:abc").expect("valid");
        assert_eq!(id.provider, ProviderId::YTMUSIC);
        assert_eq!(id.id, "abc");

        // Внутренние двоеточия — часть id, а не структура.
        let nested = parse_track_id("ytmusic:video:abc").expect("valid");
        assert_eq!(nested.id, "video:abc");
    }

    #[test]
    fn unknown_provider_error_lists_known() {
        let err = parse_track_id("nosuch:abc").expect_err("must fail");
        let text = err.to_string();
        assert!(text.contains("nosuch"), "текст: {text}");
        assert!(text.contains("ytmusic"), "текст: {text}");
        assert!(text.contains("soundcloud"), "текст: {text}");
    }

    #[test]
    fn signed_seek_is_relative_bare_is_absolute() {
        assert!(matches!(parse_seek("+15").expect("ok"), Cmd::SeekBy { delta } if delta == 15.0));
        assert!(matches!(parse_seek("-15").expect("ok"), Cmd::SeekBy { delta } if delta == -15.0));
        assert!(matches!(parse_seek("90").expect("ok"), Cmd::Seek { position } if position == Duration::from_secs(90)));
    }

    #[test]
    fn signed_volume_is_relative_and_clamped() {
        assert!(matches!(parse_vol("+5", 10.0).expect("ok"), Cmd::SetVolume { volume } if volume == 15.0));
        assert!(matches!(parse_vol("-5", 10.0).expect("ok"), Cmd::SetVolume { volume } if volume == 5.0));
        // Зажим на границах: громкость не уходит за 0 и 100.
        assert!(matches!(parse_vol("+5", 99.0).expect("ok"), Cmd::SetVolume { volume } if volume == 100.0));
        assert!(matches!(parse_vol("-5", 3.0).expect("ok"), Cmd::SetVolume { volume } if volume == 0.0));
        assert!(matches!(parse_vol("40", 10.0).expect("ok"), Cmd::SetVolume { volume } if volume == 40.0));
    }

    #[test]
    fn vol_and_seek_negative_values_are_values_not_flags() {
        // Регрессия 19.09: `tmus vol -5` падал «unexpected argument»,
        // потому что clap разбирал минус как флаг — vol-down панели
        // был мёртв, громкость росла, но не падала.
        let vol = Cli::try_parse_from(["tmus", "vol", "-5"]).expect("минус — значение, не флаг");
        assert!(matches!(vol.cmd, Some(CliCmd::Vol { value }) if value == "-5"));
        let seek = Cli::try_parse_from(["tmus", "seek", "-30"]).expect("минус — значение, не флаг");
        assert!(matches!(seek.cmd, Some(CliCmd::Seek { seconds }) if seconds == "-30"));
    }

    #[test]
    fn source_arg_maps_all_to_none_else_provider() {
        assert_eq!(parse_source_arg("all").expect("ok"), None);
        assert_eq!(parse_source_arg("ytmusic").expect("ok"), Some("ytmusic".to_owned()));
    }

    #[test]
    fn provider_flag_maps_all_to_none_else_provider() {
        assert_eq!(parse_provider_flag("all"), None);
        assert_eq!(parse_provider_flag("soundcloud"), Some("soundcloud".to_owned()));
    }

    /// Контракт `home`: провайдер и json — флаги, оба по умолчанию пусты.
    #[test]
    fn home_subcommand_parses_flags() {
        match Cli::try_parse_from(["tmus", "home"]).expect("valid").cmd {
            Some(CliCmd::Home { provider, json }) => {
                assert_eq!(provider, None);
                assert!(!json);
            }
            _ => panic!("ожидалась подкоманда home"),
        }
        match Cli::try_parse_from(["tmus", "home", "--provider", "ytmusic", "--json"])
            .expect("valid")
            .cmd
        {
            Some(CliCmd::Home { provider, json }) => {
                assert_eq!(provider.as_deref(), Some("ytmusic"));
                assert!(json);
            }
            _ => panic!("ожидалась подкоманда home с флагами"),
        }
    }

    /// Контракт `queue-goto`: индекс позиционный, 0-базный, без флагов.
    #[test]
    fn queue_goto_parses_index() {
        match Cli::try_parse_from(["tmus", "queue-goto", "3"]).expect("valid").cmd {
            Some(CliCmd::QueueGoto { index }) => assert_eq!(index, 3),
            _ => panic!("ожидалась подкоманда queue-goto"),
        }
        assert!(Cli::try_parse_from(["tmus", "queue-goto", "x"]).is_err());
    }

    #[test]
    fn loop_mode_parses_three_names_only() {
        assert_eq!(parse_loop("none").expect("ok"), LoopMode::None);
        assert_eq!(parse_loop("track").expect("ok"), LoopMode::Track);
        assert_eq!(parse_loop("queue").expect("ok"), LoopMode::Queue);
        assert!(parse_loop("forever").is_err());
    }

    #[test]
    fn rating_parses_three_names_only() {
        assert_eq!(parse_rating("like").expect("ok"), Rating::Liked);
        assert_eq!(parse_rating("dislike").expect("ok"), Rating::Disliked);
        assert_eq!(parse_rating("none").expect("ok"), Rating::None);
        assert!(parse_rating("liked").is_err());
        assert!(parse_rating("").is_err());
    }

    /// Контракт команды `rate`: track в формате TrackId, rating словом.
    #[test]
    fn rate_subcommand_parses_track_and_rating() {
        let cmd = Cli::try_parse_from(["tmus", "rate", "ytmusic:abc", "dislike"]).expect("valid");
        match cmd.cmd {
            Some(CliCmd::Rate { track, rating }) => {
                assert_eq!(parse_track_id(&track).expect("valid").id, "abc");
                assert_eq!(parse_rating(&rating).expect("valid"), Rating::Disliked);
            }
            _ => panic!("ожидалась подкоманда rate"),
        }
        // rating — свободная строка, невалидное слово ловит parse_rating
        // уже после разбора argv (как у loop/shuffle).
        assert!(parse_rating("meh").is_err());
    }

    /// Контракт `pl`: пять подкоманд, id-строки доходят до тех же
    /// парсеров, что и у соседних команд; `ls` знает `--json`.
    #[test]
    fn pl_subcommands_parse_ids_title_and_json_flag() {
        match Cli::try_parse_from(["tmus", "pl", "new", "Chill"]).expect("valid").cmd {
            Some(CliCmd::Pl(PlCmd::New { title })) => assert_eq!(title, "Chill"),
            _ => panic!("ожидалась подкоманда pl new"),
        }
        match Cli::try_parse_from(["tmus", "pl", "add", "ytmusic:PL1", "ytmusic:abc"])
            .expect("valid")
            .cmd
        {
            Some(CliCmd::Pl(PlCmd::Add { playlist_id, track })) => {
                assert_eq!(parse_playlist_id(&playlist_id).expect("valid").id, "PL1");
                assert_eq!(parse_track_id(&track).expect("valid").id, "abc");
            }
            _ => panic!("ожидалась подкоманда pl add"),
        }
        match Cli::try_parse_from(["tmus", "pl", "rm", "ytmusic:PL1", "ytmusic:abc"])
            .expect("valid")
            .cmd
        {
            Some(CliCmd::Pl(PlCmd::Rm { playlist_id, track })) => {
                assert_eq!(parse_playlist_id(&playlist_id).expect("valid").id, "PL1");
                assert_eq!(parse_track_id(&track).expect("valid").id, "abc");
            }
            _ => panic!("ожидалась подкоманда pl rm"),
        }
        match Cli::try_parse_from(["tmus", "pl", "del", "ytmusic:PL1"]).expect("valid").cmd {
            Some(CliCmd::Pl(PlCmd::Del { playlist_id })) => {
                assert_eq!(parse_playlist_id(&playlist_id).expect("valid").id, "PL1");
            }
            _ => panic!("ожидалась подкоманда pl del"),
        }
        for (args, json) in [
            (vec!["tmus", "pl", "ls"], false),
            (vec!["tmus", "pl", "ls", "--json"], true),
        ] {
            match Cli::try_parse_from(args).expect("valid").cmd {
                Some(CliCmd::Pl(PlCmd::Ls { json: got })) => assert_eq!(got, json),
                _ => panic!("ожидалась подкоманда pl ls"),
            }
        }
        // Незнакомый провайдер ловится тем же парсером id, что и везде.
        assert!(parse_playlist_id("nosuch:PL").is_err());
    }

    #[test]
    fn shuffle_parses_on_off_toggle() {
        assert_eq!(parse_shuffle("on").expect("ok"), Some(true));
        assert_eq!(parse_shuffle("off").expect("ok"), Some(false));
        assert_eq!(parse_shuffle("toggle").expect("ok"), None);
        assert!(parse_shuffle("maybe").is_err());
    }

    /// Контракт `--kind`: три вида плюс `all`, по умолчанию — треки.
    #[test]
    fn search_kind_flag_tokens_and_default() {
        let kind = |args: &[&str]| match Cli::try_parse_from(args).expect("valid").cmd {
            Some(CliCmd::Search { kind, .. }) => kind,
            _ => panic!("ожидалась подкоманда search"),
        };

        assert!(matches!(kind(&["tmus", "search", "test"]), SearchKindArg::Tracks));
        assert!(matches!(kind(&["tmus", "search", "test", "--kind", "tracks"]), SearchKindArg::Tracks));
        assert!(matches!(kind(&["tmus", "search", "test", "--kind", "artists"]), SearchKindArg::Artists));
        assert!(matches!(kind(&["tmus", "search", "test", "--kind", "playlists"]), SearchKindArg::Playlists));
        // `all` — те же три вида, спрошенные именно в этом порядке.
        assert!(matches!(kind(&["tmus", "search", "test", "--kind", "all"]), SearchKindArg::All));
        let all: &[SearchKind] = &[SearchKind::Tracks, SearchKind::Artists, SearchKind::Playlists];
        assert_eq!(SearchKindArg::All.kinds(), all);
        // `albums` есть в модели, но не в контракте команды.
        assert!(Cli::try_parse_from(["tmus", "search", "test", "--kind", "albums"]).is_err());
    }

    #[test]
    fn time_formatting() {
        assert_eq!(fmt_time(Some(187)), "3:07");
        assert_eq!(fmt_time(Some(3723)), "1:02:03");
        assert_eq!(fmt_time(None), "--:--");
    }

    /// Контракт `--band`: 1-базный номер в 0-базный индекс, дробное
    /// усиление, границы допустимы.
    #[test]
    fn band_parses_one_based_index_and_gain() {
        assert_eq!(parse_band("1:0").expect("ok"), (0, 0.0));
        assert_eq!(parse_band("10:+15").expect("ok"), (9, 15.0));
        assert_eq!(parse_band("3:-4.5").expect("ok"), (2, -4.5));
        // Границы обе включены.
        assert_eq!(parse_band("1:-15").expect("ok"), (0, -15.0));
    }

    #[test]
    fn band_rejects_out_of_range_and_garbage() {
        for bad in ["0:1", "11:1", "1:15.1", "1:-15.1", "1", "1:", "a:1", "1:x"] {
            let err = parse_band(bad).expect_err("must fail");
            let text = err.to_string();
            assert!(!text.is_empty(), "{bad}: пустой текст ошибки");
        }
    }

    /// Текст состояния: выключенный — две колонки, включённый — все
    /// десять полос с одной десятичной и ведущим знаком.
    #[test]
    fn eq_state_formats_on_and_off() {
        let mut eq = EqState::default();
        assert_eq!(format_eq(&eq), "eq off");
        eq.enabled = true;
        eq.preset = "Rock".to_owned();
        eq.bands = [5.0, 4.0, 2.0, 0.0, -1.0, 0.0, 0.0, 2.0, 4.0, 5.0];
        assert_eq!(
            format_eq(&eq),
            "eq on Rock +5.0 +4.0 +2.0 +0.0 -1.0 +0.0 +0.0 +2.0 +4.0 +5.0"
        );
    }
}
