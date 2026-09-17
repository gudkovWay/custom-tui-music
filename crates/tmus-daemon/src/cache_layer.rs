//! Прослойка офлайн-кэша над провайдером.
//!
//! Без неё кэш односторонний: файлы на диск ложатся, а воспроизведение
//! всё равно идёт в сеть — замерено на стенде (`offline=false` при
//! готовом файле в `audio/ytmusic/<id>.webm`). Хозяин заказал офлайн
//! явно, и «скачано, но не используется» этого не закрывает.
//!
//! Почему обёртка, а не правка плеера или провайдеров. Плеер знает
//! только `Registry` и не должен знать про кэш; провайдеры не должны
//! знать про него тем более — их задача добыть поток. Обёртка живёт в
//! демоне, подставляется в реестр при сборке и одинаково работает для
//! любого провайдера: это то самое требование «фичи делаем глобально,
//! а не вокруг библиотеки».

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tmus_core::cache::Cache;
use tmus_core::model::{ProviderId, StreamSource, TrackId};
use tmus_provider::{Account, Catalog, Provider, Resolver, Result};

/// Провайдер, у которого резолв сначала смотрит в офлайн-кэш.
pub struct Cached {
    inner: Arc<dyn Provider>,
    resolver: CachedResolver,
}

impl Cached {
    pub fn wrap(inner: Arc<dyn Provider>, cache: Arc<Mutex<Cache>>) -> Arc<dyn Provider> {
        let resolver = CachedResolver {
            provider: inner.id(),
            inner: Arc::clone(&inner),
            cache,
        };
        Arc::new(Self { inner, resolver })
    }
}

impl Provider for Cached {
    fn account(&self) -> &dyn Account {
        self.inner.account()
    }

    fn catalog(&self) -> &dyn Catalog {
        self.inner.catalog()
    }

    fn resolver(&self) -> &dyn Resolver {
        &self.resolver
    }
}

struct CachedResolver {
    provider: ProviderId,
    inner: Arc<dyn Provider>,
    cache: Arc<Mutex<Cache>>,
}

#[async_trait]
impl Resolver for CachedResolver {
    fn provider(&self) -> ProviderId {
        self.provider
    }

    async fn resolve(&self, track: &TrackId) -> Result<StreamSource> {
        // Замок берётся и отпускается до `await`: `Cache` синхронный, а
        // `std::sync::MutexGuard` через `await` протаскивать нельзя.
        let local = {
            let cache = self.cache.lock().expect("замок кэша отравлен паникой");
            cache.lookup_audio(track)
        };

        match local {
            Ok(Some(path)) => {
                tracing::debug!(track = %track, "играю из офлайн-кэша");
                return Ok(StreamSource::Local(path));
            }
            Ok(None) => {}
            // Сломанный кэш — не причина не играть: идём в сеть.
            Err(err) => tracing::warn!(track = %track, %err, "кэш не опрашивается"),
        }

        self.inner.resolver().resolve(track).await
    }
}
