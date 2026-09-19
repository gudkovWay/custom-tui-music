use std::sync::Arc;
use std::time::{Duration, Instant};

use tmus_core::model::{
    CatalogShelf, Playlist, PlaylistId, Rating, SearchKind, SearchResult, Track, TrackId,
};
use tmus_core::protocol::{CatalogSource, Event, ProviderView};

use crate::app::App;

/// Сколько состав плейлиста считается свежим без похода в сеть. TUI
/// дёргает состав на каждый шаг курсора по библиотеке, и без окна
/// свежести каждый заход — секунды ожидания InnerTube.
const PLAYLIST_TTL_SECS: i64 = 600;

/// Потолок страниц при ПОЛНОМ перечитывании состава плейлиста.
/// Зеркалит `MAX_PAGES` провайдера: batch-путь — страховка от вечного
/// цикла, а не замена ленивой догрузке. Если все страницы взяты, а токен
/// остался, он сохраняется — хвост дозагружается через
/// `playlist_tracks_page`.
const PLAYLIST_BATCH_PAGES: usize = 10;

/// Сколько домашняя лента считается свежей без похода в сеть (ротация
/// рекомендаций терпит устаревание).
const HOME_TTL_SECS: u64 = 600;

/// Снимок домашней ленты с ключом провайдера: повторный `Cmd::Home` с тем
/// же разрезом в пределах TTL отвечает из кэша, а не из сети.
pub(crate) struct HomeCache {
    key: String,
    at: Instant,
    shelves: Vec<CatalogShelf>,
}

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
        // Полное чтение идёт через постраничный API, а не через
        // `playlist_tracks`: только так после последней взятой страницы
        // остаётся токен, и хвост плейлиста (LM длиннее тысячи треков
        // не влезает в батч) доступен ленивой догрузке.
        let catalog = provider.catalog();
        let mut tracks = Vec::new();
        let mut cursor: Option<String> = None;
        let mut last_token: Option<String> = None;
        let mut failure = None;
        for _ in 0..PLAYLIST_BATCH_PAGES {
            match catalog.playlist_tracks_page(id, cursor.as_deref()).await {
                Ok(page) => {
                    last_token = page.next.clone();
                    cursor = page.next;
                    tracks.extend(page.tracks);
                    // Провайдеры без пагинации (дефолт трейта) отдают
                    // всё сразу с next=None — цикл кончается первой же
                    // итерацией.
                    if last_token.is_none() {
                        break;
                    }
                }
                Err(err) => {
                    failure = Some(err);
                    break;
                }
            }
        }
        match failure {
            None => {
                // put_playlist_tracks теперь ещё и ставит метку
                // свежести — следующий заход попадёт в TTL-ветку выше;
                // курсор поверх неё фиксирует полноту среза:
                // Some(токен) — есть хвост, None — дочитано.
                self.with_cache(|c| {
                    c.put_playlist_tracks(id, &tracks)?;
                    c.put_playlist_continuation(
                        id.provider.as_str(),
                        id,
                        last_token.as_deref(),
                        tracks.len(),
                    )
                })?;
                Ok(tracks)
            }
            Some(err) => {
                tracing::warn!(playlist = %id, %err, "плейлист не прочитался, беру из кэша");
                self.report_auth(provider, &err);
                if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
                    self.try_reauth(provider).await;
                }
                Ok(self.with_cache(|c| c.playlist_tracks(id))?)
            }
        }
    }

    /// Одна страница ленивой догрузки состава. Пустой ответ (без треков
    /// и без курсора) значит «дочитано или не начато»: панель по нему
    /// прекращает догрузку.
    pub(crate) async fn playlist_tracks_page(
        &self,
        id: &PlaylistId,
    ) -> anyhow::Result<(Vec<Track>, Option<String>)> {
        let provider = self
            .registry
            .get(id.provider)
            .ok_or_else(|| anyhow::anyhow!("провайдер {} не подключён", id.provider))?;
        // Нет курсора — догрузить нечего: страница была бы пустой, а
        // сеть дёргать зря не хочется.
        let Some(cursor) = self.with_cache(|c| c.playlist_continuation(id.provider.as_str(), id))?
        else {
            return Ok((Vec::new(), None));
        };
        let page = provider
            .catalog()
            .playlist_tracks_page(id, Some(&cursor))
            .await?;
        // Отдельного счётчика fetched в кэше наружу не выставлено —
        // берём текущий размер сохранённого среза: append дописывает в
        // его конец, так что это и есть число уже подтянутых треков.
        let fetched =
            self.with_cache(|c| c.playlist_tracks(id))?.len() + page.tracks.len();
        self.with_cache(|c| {
            c.append_playlist_tracks(id.provider.as_str(), id, &page.tracks)?;
            c.put_playlist_continuation(
                id.provider.as_str(),
                id,
                page.next.as_deref(),
                fetched,
            )
        })?;
        Ok((page.tracks, page.next))
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

    /// Домашняя лента рекомендаций с TTL-кэшем и single-flight.
    ///
    /// Замок `tokio::sync::Mutex` держится через весь сетевой заход:
    /// параллельные `Cmd::Home` сливаются в один, а не дёргают
    /// InnerTube горсткой. Протухшие полки не отдаём — сервис хранит
    /// прошлую ленту у себя, честная ошибка лучше несвежих советов.
    pub(crate) async fn home(&self, provider: Option<&str>) -> anyhow::Result<Vec<CatalogShelf>> {
        let key = provider.unwrap_or("all").to_owned();
        let mut guard = self.home_cache.lock().await;
        if let Some(cached) = guard.as_ref() {
            if cached.key == key && cached.at.elapsed().as_secs() < HOME_TTL_SECS {
                return Ok(cached.shelves.clone());
            }
        }
        let mut out = Vec::new();
        for target in self.targets(provider)? {
            match target.catalog().home().await {
                Ok(shelves) => out.extend(shelves),
                Err(tmus_provider::ProviderError::Unsupported { .. }) => {
                    // Не каждый провайдер умеет домашнюю ленту: это не
                    // сбой, просто полки у него нет.
                    tracing::debug!(provider = %target.id(), "домашней ленты нет");
                }
                Err(err) => {
                    tracing::warn!(provider = %target.id(), %err, "домашняя лента не прочиталась");
                    self.report_auth(target, &err);
                    if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
                        self.try_reauth(target).await;
                    }
                    // Без stale-фолбэка: ошибки уходят клиенту целиком.
                    return Err(err.into());
                }
            }
        }
        let results: Vec<SearchResult> = out.iter().flat_map(|s| s.items.iter().cloned()).collect();
        self.remember(&results)?;
        *guard = Some(HomeCache { key, at: Instant::now(), shelves: out.clone() });
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

    /// Провайдер для плейлистных операций без явного адресата
    /// (`PlaylistCreate`): берётся выбранный источник каталога, а без
    /// выбора — единственный/первый подключённый. Create — единственная
    /// операция, у которой нет id плейлиста, по которому можно понять
    /// провайдера.
    fn playlist_provider(
        &self,
    ) -> anyhow::Result<&Arc<dyn tmus_provider::Provider>> {
        let saved: Option<String> = self
            .catalog_source
            .lock()
            .expect("catalog source")
            .provider
            .clone();
        match saved {
            Some(name) => self
                .registry
                .get_by_str(&name)
                .ok_or_else(|| anyhow::anyhow!("провайдер {name} не подключён")),
            None => self
                .registry
                .iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("нет подключённых провайдеров")),
        }
    }

    /// Единый хвост плейлистных мутаций: auth-ошибки уходят в отчёт и
    /// возможный reauth (как у `rate_track`), остальные — наружу в
    /// стандартном формате ошибок демона. Reauth не чинит текущий
    /// запрос — ошибка всё равно идёт клиенту, но следующая пройдёт
    /// уже со свежей сессией.
    async fn report_playlist_error(
        &self,
        provider: &Arc<dyn tmus_provider::Provider>,
        err: tmus_provider::ProviderError,
    ) -> anyhow::Error {
        tracing::warn!(provider = %provider.id(), %err, "плейлистная операция не принята");
        self.report_auth(provider, &err);
        if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
            self.try_reauth(provider).await;
        }
        err.into()
    }

    /// Создать плейлист у выбранного провайдера и разослать сигнал.
    pub(crate) async fn playlist_create(&self, title: &str) -> anyhow::Result<Playlist> {
        let provider = self.playlist_provider()?;
        match provider.catalog().playlist_create(title).await {
            Ok(playlist) => {
                self.emit(Event::PlaylistsChanged);
                Ok(playlist)
            }
            Err(err) => Err(self.report_playlist_error(provider, err).await),
        }
    }

    /// Добавить трек в плейлист и разослать сигнал.
    pub(crate) async fn playlist_add(
        &self,
        playlist: &PlaylistId,
        track: &TrackId,
    ) -> anyhow::Result<()> {
        let provider = self.registry.get(playlist.provider).ok_or_else(|| {
            anyhow::anyhow!("провайдер {} не подключён", playlist.provider)
        })?;
        match provider.catalog().playlist_add(playlist, track).await {
            Ok(()) => {
                // Состав плейлиста изменился на сервере — TTL-кэш треков
                // обязан умереть немедленно: иначе повторное открытие
                // плейлиста до `PLAYLIST_TTL_SECS` показывает состав
                // БЕЗ добавленного трека (живой замер 19.09).
                self.with_cache(|c| c.forget_playlist_sync(playlist))?;
                self.emit(Event::PlaylistsChanged);
                Ok(())
            }
            Err(err) => Err(self.report_playlist_error(provider, err).await),
        }
    }

    /// Убрать трек из плейлиста и разослать сигнал.
    pub(crate) async fn playlist_remove(
        &self,
        playlist: &PlaylistId,
        track: &TrackId,
    ) -> anyhow::Result<()> {
        let provider = self.registry.get(playlist.provider).ok_or_else(|| {
            anyhow::anyhow!("провайдер {} не подключён", playlist.provider)
        })?;
        match provider.catalog().playlist_remove(playlist, track).await {
            Ok(()) => {
                // Симметрично add: удалённый трек не должен доживать
                // свой TTL в кэше.
                self.with_cache(|c| c.forget_playlist_sync(playlist))?;
                self.emit(Event::PlaylistsChanged);
                Ok(())
            }
            Err(err) => Err(self.report_playlist_error(provider, err).await),
        }
    }


    /// Удалить плейлист и разослать сигнал.
    pub(crate) async fn playlist_delete(&self, playlist: &PlaylistId) -> anyhow::Result<()> {
        let provider = self.registry.get(playlist.provider).ok_or_else(|| {
            anyhow::anyhow!("провайдер {} не подключён", playlist.provider)
        })?;
        match provider.catalog().playlist_delete(playlist).await {
            Ok(()) => {
                // Плейлист исчез: его состав в кэше больше не отвечает
                // реальности — сбрасываем метку свежести.
                self.with_cache(|c| c.forget_playlist_sync(playlist))?;
                self.emit(Event::PlaylistsChanged);
                Ok(())
            }
            Err(err) => Err(self.report_playlist_error(provider, err).await),
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
