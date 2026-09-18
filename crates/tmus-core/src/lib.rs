//! Ядро: провайдер-независимые типы, протокол демона, пути, конфиг,
//! учётные данные и офлайн-кэш.
//!
//! Границы крейта заданы одним правилом: **здесь не должно быть ни
//! одного упоминания конкретного сервиса**, кроме констант
//! `ProviderId`. Ни InnerTube, ни SoundCloud, ни Spotify. Всё
//! сервис-специфичное живёт в своём крейте провайдера и наружу отдаёт
//! только `Track`, `Playlist`, `StreamSource`.
//!
//! Причина — замеренная: InnerTube неофициален и ломается без
//! предупреждения. Пока его структуры не видны никому, кроме
//! `tmus-ytmusic`, поломка провайдера остаётся поломкой провайдера.

pub mod cache;
pub mod catalog_source;
pub mod config;
pub mod cookies;
pub mod error;
pub mod model;
pub mod paths;
pub mod protocol;

pub use error::{CoreError, Result};
pub use model::{
    AuthStatus, LoopMode, PlaybackStatus, Playlist, PlaylistId, ProviderId, SearchKind,
    SearchResult, StreamSource, Track, TrackId,
};
pub use paths::{APP, MPRIS_BUS_NAME, Paths};
