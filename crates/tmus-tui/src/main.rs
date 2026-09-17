//! `tmus` — клиент демона. Без аргументов поднимает TUI, с подкомандой
//! делает один запрос и печатает результат; подкоманды вешаются на
//! медиа-клавиши Hyprland и дергаются плагином noctalia, поэтому каждая
//! обязана завершаться сама и не трогать терминал.

mod client;
mod ui;

use std::time::Duration;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use tmus_core::model::{PlaylistId, ProviderId, TrackId};
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
    /// Играть трек (по умолчанию — текущий/первый из очереди).
    Play { track_id: Option<String> },
    Pause,
    Toggle,
    Stop,
    Next,
    Prev,
    /// Позиция в секундах: со знаком — относительный сдвиг.
    Seek { seconds: String },
    /// Громкость 0..100: со знаком — от текущей.
    Vol { value: String },
    /// Состояние плеера.
    Status {
        #[arg(long)]
        json: bool,
    },
    Providers {
        #[arg(long)]
        json: bool,
    },
    Library {
        #[arg(long)]
        json: bool,
    },
    Liked {
        #[arg(long)]
        json: bool,
    },
    Queue {
        #[arg(long)]
        json: bool,
    },
    Search {
        query: String,
        #[arg(long)]
        json: bool,
    },
    /// Играть плейлист с начала.
    PlayPlaylist { playlist_id: String },
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
        CliCmd::Status { json } => return print_payload(client.call(Cmd::State).await?, json),
        CliCmd::Providers { json } => return print_payload(client.call(Cmd::Providers).await?, json),
        CliCmd::Library { json } => return print_payload(client.call(Cmd::Library { provider: None }).await?, json),
        CliCmd::Liked { json } => return print_payload(client.call(Cmd::Liked { provider: None }).await?, json),
        CliCmd::Queue { json } => return print_payload(client.call(Cmd::Queue).await?, json),
        CliCmd::Search { query, json } => {
            return print_payload(
                client.call(Cmd::Search { query, kind: tmus_core::model::SearchKind::Tracks, provider: None }).await?,
                json,
            );
        }
        CliCmd::PlayPlaylist { playlist_id } => Cmd::PlayPlaylist {
            playlist: parse_playlist_id(&playlist_id)?,
            start: Some(0),
        },
        CliCmd::Cache(cache) => match cache {
            CacheCmd::Stats { json } => return print_payload(client.call(Cmd::CacheStats).await?, json),
            CacheCmd::Pin { track_id } => {
                Cmd::CachePin { tracks: vec![parse_track_id(&track_id)?] }
            }
            CacheCmd::Unpin { track_id } => {
                Cmd::CacheUnpin { tracks: vec![parse_track_id(&track_id)?] }
            }
            CacheCmd::Gc => Cmd::CacheGc,
        },
        CliCmd::StopDaemon => Cmd::Shutdown,
    };

    print_payload(client.call(cmd).await?, false)
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
            .map(|r| match r {
                tmus_core::model::SearchResult::Track(t) => {
                    format!("{}: {} — {}", t.id, t.artist_line(), t.title)
                }
                tmus_core::model::SearchResult::Playlist(p) => format!("{}: {}", p.id, p.title),
                tmus_core::model::SearchResult::Artist { provider, id, name } => {
                    format!("{provider}:{id}: {name}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Payload::Playlists(playlists) => playlists
            .iter()
            .map(|p| format!("{}: {}", p.id, p.title))
            .collect::<Vec<_>>()
            .join("\n"),
        Payload::Providers(providers) => providers
            .iter()
            .map(|p| format!("{} ({}): {:?}", p.id, p.name, p.auth))
            .collect::<Vec<_>>()
            .join("\n"),
        Payload::Cache(c) => format!(
            "треков {} ({:.1} МиБ), закреплено {} ({:.1} МиБ), лимит {:.1} МиБ",
            c.tracks,
            c.bytes as f64 / 1048576.0,
            c.pinned_tracks,
            c.pinned_bytes as f64 / 1048576.0,
            c.limit_bytes as f64 / 1048576.0,
        ),
    }
}

async fn run_events(paths: &Paths) -> Result<()> {
    let mut rx = client::Client::subscribe(paths).await?;
    while let Some(event) = rx.recv().await {
        // По одному JSON на строку с flush: потребитель — runStream
        // плагина, без flush бар обновляется рывками по буферу.
        println!("{}", serde_json::to_string(&event)?);
        use std::io::Write as _;
        std::io::stdout().flush()?;
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
    fn time_formatting() {
        assert_eq!(fmt_time(Some(187)), "3:07");
        assert_eq!(fmt_time(Some(3723)), "1:02:03");
        assert_eq!(fmt_time(None), "--:--");
    }
}
