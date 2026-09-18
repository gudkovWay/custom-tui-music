use std::sync::Arc;
use std::time::{Duration, Instant};

use tmus_core::model::{Playlist, PlaylistId, Rating, SearchKind, SearchResult, Track, TrackId};
use tmus_core::protocol::{CatalogSource, Event, ProviderView};

use crate::app::App;

/// Сколько состав плейлиста считается свежим без похода в сеть. TUI
/// дёргает состав на каждый шаг курсора по библиотеке, и без окна
/// свежести каждый заход — секунды ожидания InnerTube.
const PLAYLIST_TTL_SECS: i64 = 600;

/// Кулдаун попыток перечитать cookies браузера: одна на провайдера,
/// чтобы ливень auth-ошибок не превратился в ливень перечитываний
/// профиля.
const REAUTH_COOLDOWN: Duration = Duration::from_secs(300);

impl App {
    pub(crate) fn providers(&self) -> Vec<ProviderView> {
        self.registry
            .iter()
            .map(|provider| {
                let account = provider.account();
                ProviderView {
                    id: provider.id().as_str().to_owned(),
                    name: account.display_name().to_owned(),
                    auth: account.auth(),
                }
            })
            .collect()
    }

    /// Провайдеры под запрос: назван один — только он, не назван — все.
    pub(crate) fn targets(&self, provider: Option<&str>) -> anyhow::Result<Vec<&Arc<dyn tmus_provider::Provider>>> {
        match provider {
            Some(name) => {
                let one = self
                    .registry
                    .get_by_str(name)
                    .ok_or_else(|| anyhow::anyhow!("провайдер {name} не подключён"))?;
                Ok(vec![one])
            }
            None => Ok(self.registry.iter().collect()),
        }
    }

    pub(crate) async fn search(
        &self,
        query: &str,
        kind: SearchKind,
        provider: Option<&str>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let mut out = Vec::new();
        for target in self.targets(provider)? {
            // Один упавший провайдер не должен обнулять поиск по
            // остальным: агрегирующий поиск тем и полезен.
            match target.catalog().search(query, kind).await {
                Ok(found) => out.extend(found),
                Err(err) => {
                    tracing::warn!(provider = %target.id(), %err, "поиск не удался");
                    self.report_auth(target, &err);
                    if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
                        self.try_reauth(target).await;
                    }
                }
            }
        }
        self.remember(&out)?;
        Ok(out)
    }

    pub(crate) async fn library(&self, provider: Option<&str>) -> anyhow::Result<Vec<Playlist>> {
        let mut out = Vec::new();
        for target in self.targets(provider)? {
            match target.catalog().playlists().await {
                Ok(found) => out.extend(found),
                Err(err) => {
                    tracing::warn!(provider = %target.id(), %err, "библиотека не прочиталась");
                    self.report_auth(target, &err);
                    if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
                        self.try_reauth(target).await;
                    }
                }
            }
        }
        if out.is_empty() {
            // Сеть могла отвалиться целиком — тогда показываем то, что
            // уже знаем. Это половина смысла офлайн-кэша.
            out = self.with_cache(|c| c.playlists(provider))?;
        } else {
            self.with_cache(|c| c.put_playlists(&out))?;
        }
        Ok(out)
    }

    pub(crate) async fn playlist_tracks(&self, id: &PlaylistId) -> anyhow::Result<Vec<Track>> {
        let provider = self
            .registry
            .get(id.provider)
            .ok_or_else(|| anyhow::anyhow!("провайдер {} не подключён", id.provider))?;
        // Свежий кэш отвечает мгновенно: состав нужен на каждый шаг
        // курсора, и сеть там не пережить.
        if let Some(tracks) = self.with_cache(|c| c.playlist_tracks_if_fresh(id, PLAYLIST_TTL_SECS))? {
            return Ok(tracks);
        }
        match provider.catalog().playlist_tracks(id).await {
            Ok(tracks) => {
                // put_playlist_tracks теперь ещё и ставит метку
                // свежести — следующий заход попадёт в TTL-ветку выше.
                self.with_cache(|c| c.put_playlist_tracks(id, &tracks))?;
                Ok(tracks)
            }
            Err(err) => {
                tracing::warn!(playlist = %id, %err, "плейлист не прочитался, беру из кэша");
                self.report_auth(provider, &err);
                if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
                    self.try_reauth(provider).await;
                }
                Ok(self.with_cache(|c| c.playlist_tracks(id))?)
            }
        }
    }

    pub(crate) async fn liked(&self, provider: Option<&str>) -> anyhow::Result<Vec<Track>> {
        let mut out = Vec::new();
        for target in self.targets(provider)? {
            match target.catalog().liked().await {
                Ok(found) => out.extend(found),
                Err(err) => {
                    tracing::warn!(provider = %target.id(), %err, "лайки не прочитались");
                    self.report_auth(target, &err);
                    if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
                        self.try_reauth(target).await;
                    }
                }
            }
        }
        if !out.is_empty() {
            self.with_cache(|c| c.put_tracks(&out))?;
        }
        Ok(out)
    }

    /// Поставить оценку треку: локально сразу, у провайдера — в сеть.
    ///
    /// Порядок намеренно оптимистичный: кэш пишется ДО сетевого вызова,
    /// чтобы панель скрыла/показала трек мгновенно; при ошибке сети
    /// локальное состояние откатывается, и человек видит ошибку, а не
    /// расхождение панели с сервером.
    pub(crate) async fn rate_track(
        &self,
        id: &TrackId,
        rating: Rating,
    ) -> anyhow::Result<()> {
        let past = self.with_cache(|c| c.get_rating(id))?;
        self.with_cache(|c| c.set_rating(id, rating))?;

        let provider = self.registry.get(id.provider).ok_or_else(|| {
            anyhow::anyhow!("провайдер {} не подключён", id.provider)
        })?;
        match provider.catalog().rate(id, rating).await {
            Ok(()) => {
                self.emit(Event::RatingChanged { track: id.clone(), rating });
                // Скрытие играющего дизлайком должно быть немедленным:
                // человек как раз хочет, чтобы это ушло из ушей сейчас.
                // Тот же `step`, что и у `Cmd::Next`: одна логика
                // перехода на все пути.
                if rating == Rating::Disliked {
                    let current = self.player.state().await.track.map(|t| t.id);
                    if current.as_ref() == Some(id) {
                        self.step(true).await?;
                    }
                }
                if rating == Rating::Liked {
                    // Лайк улетел на сервер — плейлист лайкнутого там уже
                    // изменился, и TTL-кэш его состава больше не правда.
                    let liked_playlist = PlaylistId::new(id.provider, "LM");
                    self.with_cache(|c| c.forget_playlist_sync(&liked_playlist))?;
                }
                Ok(())
            }
            Err(err) => {
                tracing::warn!(provider = %provider.id(), %err, "оценка не принята");
                self.report_auth(provider, &err);
                if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
                    self.try_reauth(provider).await;
                }
                // Откат: `past = None` даёт `Rating::None`, то есть
                // удаление строки — оптимистичная запись не оставляет
                // ложного следа.
                self.with_cache(|c| c.set_rating(id, past.unwrap_or(Rating::None)))?;
                Err(err.into())
            }
        }
    }

    pub(crate) fn remember(&self, results: &[SearchResult]) -> anyhow::Result<()> {
        let tracks: Vec<Track> = results
            .iter()
            .filter_map(|r| match r {
                SearchResult::Track(track) => Some(track.clone()),
                _ => None,
            })
            .collect();
        if !tracks.is_empty() {
            self.with_cache(|c| c.put_tracks(&tracks))?;
        }
        Ok(())
    }

    /// Отвалившаяся авторизация обязана дойти до бара событием, а не
    /// всплыть позже невнятным «bot check» при попытке что-то включить.
    pub(crate) fn report_auth(&self, provider: &Arc<dyn tmus_provider::Provider>, err: &tmus_provider::ProviderError) {
        if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
            self.emit(Event::AuthChanged {
                provider: provider.id().as_str().to_owned(),
                auth: provider.account().auth(),
            });
        }
    }

    /// Раз в `REAUTH_COOLDOWN` на провайдера: перечитать cookies
    /// браузера и проверить сессию. Человек мог перезалогиниться ещё до
    /// того, как демон заметил протухание — молчать об этом значило бы
    /// требовать рестарт демона.
    async fn try_reauth(&self, provider: &Arc<dyn tmus_provider::Provider>) {
        let id = provider.id().as_str().to_owned();
        // Кулдаун под замком: вторая параллельная auth-ошибка того же
        // провайдера не должна вторгаться в профиль браузера следом.
        {
            let mut retries = self.auth_retry.lock().expect("замок auth_retry");
            if retries.get(&id).is_some_and(|at| at.elapsed() < REAUTH_COOLDOWN) {
                return;
            }
            retries.insert(id.clone(), Instant::now());
        }
        // refresh ходит в сеть до 30 с; клиент ждёт ответ, поэтому дольше
        // 10 с ждать бессмысленно — отдаём управление, а не блокируемся.
        match tokio::time::timeout(Duration::from_secs(10), provider.account().refresh()).await {
            Ok(Ok(auth)) => self.emit(Event::AuthChanged { provider: id, auth }),
            Ok(Err(err)) => {
                tracing::debug!(provider = %id, %err, "перечитывание сессии не удалось");
            }
            Err(_) => {
                tracing::debug!(provider = %id, "перечитывание сессии превысило 10 с");
            }
        }
    }
}

pub fn resolve_catalog_source(
    saved: Option<&CatalogSource>,
    connected: Vec<String>,
) -> CatalogSource {
    match saved {
        Some(source)
            if source
                .provider
                .as_deref()
                .is_none_or(|provider| connected.iter().any(|known| known == provider)) =>
        {
            source.clone()
        }
        _ => CatalogSource { provider: connected.into_iter().next() },
    }
}

#[cfg(test)]
mod catalog_source_tests {
    use tmus_core::protocol::CatalogSource;

    use super::resolve_catalog_source;

    #[test]
    fn saved_provider_is_kept() {
        let saved = CatalogSource { provider: Some("soundcloud".into()) };
        assert_eq!(
            resolve_catalog_source(Some(&saved), vec!["ytmusic".into(), "soundcloud".into()]),
            saved
        );
    }

    #[test]
    fn invalid_saved_provider_falls_back_to_first_connected() {
        let saved = CatalogSource { provider: Some("spotify".into()) };
        assert_eq!(
            resolve_catalog_source(Some(&saved), vec!["ytmusic".into(), "soundcloud".into()]),
            CatalogSource { provider: Some("ytmusic".into()) }
        );
    }

    #[test]
    fn saved_all_is_kept() {
        let saved = CatalogSource { provider: None };
        assert_eq!(
            resolve_catalog_source(Some(&saved), vec!["ytmusic".into()]),
            CatalogSource { provider: None }
        );
    }

    #[test]
    fn no_saved_source_selects_first_connected_or_all() {
        assert_eq!(
            resolve_catalog_source(None, Vec::new()),
            CatalogSource { provider: None }
        );
        assert_eq!(
            resolve_catalog_source(None, vec!["soundcloud".into()]),
            CatalogSource { provider: Some("soundcloud".into()) }
        );
    }
}
