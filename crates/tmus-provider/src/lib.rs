//! Абстракция провайдеров: три трейта и реестр.
//!
//! Почему три трейта, а не один `Source`. Каталог обобщается чисто:
//! поиск, плейлисты, лайки есть у всех сервисов и выражаются одними
//! типами. Добыча потока не обобщается вовсе: YouTube Music и
//! SoundCloud резолвятся через `yt-dlp`, а Spotify так резолвиться не
//! может — там свой протокол, нужен `librespot` и Premium; у Yandex
//! Music свой API с подписанными ссылками. Один трейт на оба дела
//! заставил бы Spotify притворяться yt-dlp-провайдером.
//!
//! Правило, обязательное к соблюдению: ни `tmus-player`, ни
//! `tmus-daemon`, ни `tmus-tui` не зависят от крейтов конкретных
//! провайдеров. Провайдер приходит только через [`Registry`].

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use tmus_core::model::{
    AuthStatus, Playlist, PlaylistId, ProviderId, Rating, SearchKind, SearchResult, StreamSource,
    Track, TrackId,
};

pub mod ytdlp;

/// Ошибка провайдера.
///
/// Типы ответов сервиса сюда не протекают: наружу уходит причина, а не
/// нераспарсенный JSON. Это то же правило, что и в `tmus-core`, но с
/// другой стороны границы.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// Сессия отсутствует или отвергнута. Отдельно от `Network`, потому
    /// что чинится совсем иначе, а сервисы часто отдают одно и то же
    /// «подтвердите, что вы не робот» в обоих случаях.
    #[error("авторизация {provider}: {reason}")]
    Auth { provider: ProviderId, reason: String },

    #[error("сеть: {0}")]
    Network(String),

    /// Ответ получен, но не разбирается — сервис изменил формат.
    #[error("{provider} ответил неожиданным форматом: {reason}")]
    Format { provider: ProviderId, reason: String },

    #[error("нет такого трека: {0}")]
    NoSuchTrack(TrackId),

    #[error("нет такого плейлиста: {0}")]
    NoSuchPlaylist(PlaylistId),

    /// Провайдер не умеет этот вид запроса. Не ошибка пользователя —
    /// вызывающий обязан считать это пустым результатом там, где это
    /// уместно.
    #[error("{provider} не поддерживает {what}")]
    Unsupported { provider: ProviderId, what: &'static str },

    #[error("внешний инструмент {tool}: {reason}")]
    Tool { tool: &'static str, reason: String },
}

pub type Result<T, E = ProviderError> = std::result::Result<T, E>;

/// Аккаунт провайдера: кто мы для сервиса и жива ли сессия.
#[async_trait]
pub trait Account: Send + Sync {
    fn provider(&self) -> ProviderId;

    /// Человекочитаемое имя для UI: «YouTube Music», «SoundCloud».
    fn display_name(&self) -> &str;

    /// Текущее состояние авторизации. Дешёвая операция: состояние
    /// кэшируется, сеть не трогается.
    fn auth(&self) -> AuthStatus;

    /// Перечитать учётные данные и проверить их у сервиса.
    async fn refresh(&self) -> Result<AuthStatus>;
}

/// Каталог: всё, что читается, но не играет.
#[async_trait]
pub trait Catalog: Send + Sync {
    fn provider(&self) -> ProviderId;

    async fn search(&self, query: &str, kind: SearchKind) -> Result<Vec<SearchResult>>;

    /// Подсказки поиска. Провайдер без них возвращает пустой вектор, а
    /// не `Unsupported`: подсказки — украшение, из-за них не должен
    /// падать ввод.
    async fn suggest(&self, _query: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// Плейлисты библиотеки пользователя.
    async fn playlists(&self) -> Result<Vec<Playlist>>;

    async fn playlist_tracks(&self, playlist: &PlaylistId) -> Result<Vec<Track>>;

    /// Лайкнутое. У YouTube Music это плейлист `LM`, у других — своё;
    /// вызывающего это не касается.
    async fn liked(&self) -> Result<Vec<Track>>;

    /// Поставить оценку треку у провайдера.
    ///
    /// Дефолт обязателен: реестр мультипровайдерный, и провайдер без
    /// сетевых оценок (SoundCloud появится позже) не должен быть
    /// обязан ничего выдумывать. Ошибка [`ProviderError::Unsupported`]
    /// — не сбой: вызывающий обязан трактовать её как «поставить
    /// оценку здесь нельзя», а не падать.
    async fn rate(&self, _id: &TrackId, _rating: Rating) -> Result<()> {
        Err(ProviderError::Unsupported {
            provider: self.provider(),
            what: "оценки",
        })
    }

    /// Создать пустой плейлист у провайдера, вернуть его описание.
    ///
    /// Дефолт обязателен по той же причине, что и у [`Catalog::rate`]:
    /// реестр мультипровайдерный, и не каждый сервис даёт редактировать
    /// библиотеку аккаунта (у части провайдеров плейлисты живут только
    /// на стороне плеера). [`ProviderError::Unsupported`] — не сбой:
    /// вызывающий обязан трактовать её как «здесь плейлист не создать».
    async fn playlist_create(&self, _title: &str) -> Result<Playlist> {
        Err(ProviderError::Unsupported {
            provider: self.provider(),
            what: "создание плейлистов",
        })
    }

    /// Добавить трек в плейлист аккаунта.
    ///
    /// Дефолт `Unsupported`: редактирование плейлистов — операция над
    /// аккаунтом, и провайдер без такой возможности не должен её
    /// изображать. Вызывающий обязан считать ошибку признаком «операция
    /// для этого провайдера недоступна», а не падать.
    async fn playlist_add(&self, _playlist: &PlaylistId, _track: &TrackId) -> Result<()> {
        Err(ProviderError::Unsupported {
            provider: self.provider(),
            what: "добавление в плейлист",
        })
    }

    /// Убрать трек из плейлиста аккаунта.
    ///
    /// Дефолт `Unsupported` — см. [`Catalog::playlist_add`]: не каждый
    /// сервис умеет менять содержимое плейлистов, и обязанность
    /// выдумывать поведение у него нет.
    async fn playlist_remove(&self, _playlist: &PlaylistId, _track: &TrackId) -> Result<()> {
        Err(ProviderError::Unsupported {
            provider: self.provider(),
            what: "удаление из плейлиста",
        })
    }

    /// Удалить плейлист аккаунта целиком.
    ///
    /// Дефолт `Unsupported` — см. [`Catalog::playlist_create`]:
    /// уничтожение данных аккаунта тем более не должно изображаться
    /// провайдером, который такой операции не имеет.
    async fn playlist_delete(&self, _playlist: &PlaylistId) -> Result<()> {
        Err(ProviderError::Unsupported {
            provider: self.provider(),
            what: "удаление плейлиста",
        })
    }
}

/// Резолвер: `TrackId` → откуда играть.
///
/// Реализация обязана возвращать `StreamSource::Remote` с
/// `expires_at`, если ссылка временная. Кэшировать URL нельзя —
/// у googlevideo он живёт около шести часов.
#[async_trait]
pub trait Resolver: Send + Sync {
    fn provider(&self) -> ProviderId;

    async fn resolve(&self, track: &TrackId) -> Result<StreamSource>;
}

/// Провайдер целиком. Отдельный трейт нужен, чтобы реестр хранил один
/// `Arc` вместо трёх и не мог собрать несогласованную тройку.
pub trait Provider: Send + Sync {
    fn account(&self) -> &dyn Account;
    fn catalog(&self) -> &dyn Catalog;
    fn resolver(&self) -> &dyn Resolver;

    fn id(&self) -> ProviderId {
        self.account().provider()
    }
}

/// Реестр подключённых провайдеров.
///
/// Единственный способ, которым плеер, демон и TUI видят провайдеров.
/// Порядок обхода детерминированный (`BTreeMap` по `ProviderId`) —
/// иначе агрегирующий поиск выдавал бы результаты в разном порядке при
/// каждом запуске.
#[derive(Default, Clone)]
pub struct Registry {
    providers: BTreeMap<ProviderId, Arc<dyn Provider>>,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Добавить провайдера. Повторная регистрация того же `ProviderId`
    /// заменяет прежнего и возвращает его.
    pub fn insert(&mut self, provider: Arc<dyn Provider>) -> Option<Arc<dyn Provider>> {
        self.providers.insert(provider.id(), provider)
    }

    #[must_use]
    pub fn get(&self, id: ProviderId) -> Option<&Arc<dyn Provider>> {
        self.providers.get(&id)
    }

    /// Поиск по строковому идентификатору — так провайдер приходит из
    /// control-протокола и из конфига.
    #[must_use]
    pub fn get_by_str(&self, id: &str) -> Option<&Arc<dyn Provider>> {
        self.providers
            .iter()
            .find(|(known, _)| known.as_str() == id)
            .map(|(_, provider)| provider)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Provider>> {
        self.providers.values()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Резолвер для трека. Возвращает `NoSuchTrack`, а не `None`:
    /// «провайдер не подключён» и «трека нет» для вызывающего
    /// неразличимы и чинятся одинаково — переподключением аккаунта.
    pub fn resolver_for(&self, track: &TrackId) -> Result<&dyn Resolver> {
        self.providers
            .get(&track.provider)
            .map(|provider| provider.resolver())
            .ok_or_else(|| ProviderError::NoSuchTrack(track.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub(ProviderId);

    #[async_trait]
    impl Account for Stub {
        fn provider(&self) -> ProviderId {
            self.0
        }
        fn display_name(&self) -> &str {
            "stub"
        }
        fn auth(&self) -> AuthStatus {
            AuthStatus::Anonymous
        }
        async fn refresh(&self) -> Result<AuthStatus> {
            Ok(AuthStatus::Anonymous)
        }
    }

    #[async_trait]
    impl Catalog for Stub {
        fn provider(&self) -> ProviderId {
            self.0
        }
        async fn search(&self, _q: &str, _k: SearchKind) -> Result<Vec<SearchResult>> {
            Ok(Vec::new())
        }
        async fn playlists(&self) -> Result<Vec<Playlist>> {
            Ok(Vec::new())
        }
        async fn playlist_tracks(&self, _p: &PlaylistId) -> Result<Vec<Track>> {
            Ok(Vec::new())
        }
        async fn liked(&self) -> Result<Vec<Track>> {
            Ok(Vec::new())
        }
    }

    #[async_trait]
    impl Resolver for Stub {
        fn provider(&self) -> ProviderId {
            self.0
        }
        async fn resolve(&self, _track: &TrackId) -> Result<StreamSource> {
            Ok(StreamSource::Local("/dev/null".into()))
        }
    }

    impl Provider for Stub {
        fn account(&self) -> &dyn Account {
            self
        }
        fn catalog(&self) -> &dyn Catalog {
            self
        }
        fn resolver(&self) -> &dyn Resolver {
            self
        }
    }

    #[test]
    fn default_rate_is_unsupported() {
        // Провайдер без переопределения `rate` (будущий SoundCloud)
        // обязан отвечать Unsupported, а не падать где-то в сети.
        let stub = Stub(ProviderId::SOUNDCLOUD);
        let track = TrackId::new(ProviderId::SOUNDCLOUD, "abc");
        let error = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(stub.rate(&track, Rating::Liked))
            .unwrap_err();
        assert!(matches!(error, ProviderError::Unsupported { provider, .. } if provider == ProviderId::SOUNDCLOUD));
    }

    #[test]
    fn registry_routes_by_track_provider() {
        let mut registry = Registry::new();
        registry.insert(Arc::new(Stub(ProviderId::YTMUSIC)));
        registry.insert(Arc::new(Stub(ProviderId::SOUNDCLOUD)));

        let ytm = TrackId::new(ProviderId::YTMUSIC, "a");
        assert_eq!(
            registry.resolver_for(&ytm).expect("routed").provider(),
            ProviderId::YTMUSIC
        );

        let unknown = TrackId::new(ProviderId("spotify"), "a");
        assert!(matches!(
            registry.resolver_for(&unknown),
            Err(ProviderError::NoSuchTrack(_))
        ));
    }

    #[test]
    fn registry_iteration_order_is_deterministic() {
        let mut first = Registry::new();
        first.insert(Arc::new(Stub(ProviderId::SOUNDCLOUD)));
        first.insert(Arc::new(Stub(ProviderId::YTMUSIC)));

        let mut second = Registry::new();
        second.insert(Arc::new(Stub(ProviderId::YTMUSIC)));
        second.insert(Arc::new(Stub(ProviderId::SOUNDCLOUD)));

        let order = |registry: &Registry| {
            registry.iter().map(|p| p.id().as_str()).collect::<Vec<_>>()
        };
        assert_eq!(order(&first), order(&second));
    }

    #[test]
    fn reinserting_a_provider_replaces_it() {
        let mut registry = Registry::new();
        assert!(registry.insert(Arc::new(Stub(ProviderId::YTMUSIC))).is_none());
        assert!(registry.insert(Arc::new(Stub(ProviderId::YTMUSIC))).is_some());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn lookup_by_string_matches_the_protocol_spelling() {
        let mut registry = Registry::new();
        registry.insert(Arc::new(Stub(ProviderId::YTMUSIC)));
        assert!(registry.get_by_str("ytmusic").is_some());
        assert!(registry.get_by_str("YTMusic").is_none());
    }
}
