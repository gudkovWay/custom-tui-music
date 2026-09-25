use std::sync::Arc;
use std::time::{Duration, Instant};

use tmus_core::model::{
    CatalogShelf, Playlist, PlaylistId, ProviderId, Rating, SearchKind, SearchResult, Track, TrackId,
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
                    glyph: account.glyph().to_owned(),
                    color: account.color().to_owned(),
                    capabilities: account.capabilities(),
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
        // Параллельный fan-out, а не последовательный for: сумма
        // латентностей провайдеров при мультисессии (YTM + SC) делает
        // поиск мучительным, join_all ждал бы всех одновременно, а
        // порядок результатов остаётся детерминированным (порядок
        // Registry).
        let targets = self.targets(provider)?;
        let results = futures_util::future::join_all(
            targets
                .iter()
                .map(|target| async move { (*target, target.catalog().search(query, kind).await) }),
        )
        .await;
        let mut out = Vec::new();
        for (target, result) in results {
            // Один упавший провайдер не должен обнулять поиск по
            // остальным: агрегирующий поиск тем и полезен.
            match result {
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
        // Явный local — только плейлисты приложения, сеть не трогаем.
        if provider == Some(ProviderId::LOCAL.as_str()) {
            return self.with_cache(|c| c.local_playlists());
        }
        // Смешанный запрос (None) начинается с плейлистов приложения:
        // они всегда доступны, даже когда сеть лежит целиком.
        let locals = match provider {
            None => self.with_cache(|c| c.local_playlists())?,
            Some(_) => Vec::new(),
        };
        // Fan-out параллельно: поиск и библиотека — самые частые
        // мультисессионные запросы, последовательная сумма латентностей
        // недопустима.
        let targets = self.targets(provider)?;
        let results = futures_util::future::join_all(
            targets
                .iter()
                .map(|target| async move { (*target, target.catalog().playlists().await) }),
        )
        .await;
        let mut remote = Vec::new();
        for (target, result) in results {
            match result {
                Ok(found) => remote.extend(found),
                Err(err) => {
                    tracing::warn!(provider = %target.id(), %err, "библиотека не прочиталась");
                    self.report_auth(target, &err);
                    if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
                        self.try_reauth(target).await;
                    }
                }
            }
        }
        // Локальные плейлисты в кэше провайдерских не живут, поэтому
        // stale-фолбэк касается только удалённой части.
        if remote.is_empty() {
            // Сеть могла отвалиться целиком — тогда показываем то, что
            // уже знаем. Это половина смысла офлайн-кэша.
            let cached = self.with_cache(|c| c.playlists(provider))?;
            remote.extend(cached);
        } else {
            self.with_cache(|c| c.put_playlists(&remote))?;
        }
        let mut out = locals;
        out.extend(remote);
        Ok(out)
    }

    pub(crate) async fn playlist_tracks(&self, id: &PlaylistId) -> anyhow::Result<Vec<Track>> {
        // Локальный плейлист живёт целиком в кэше: метаданные записаны
        // при добавлении, Registry не при делах.
        if id.provider == ProviderId::LOCAL {
            return self.with_cache(|c| c.local_playlist_tracks(id));
        }
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
        // Локальный состав всегда прочитан целиком: догрузить нечего.
        if id.provider == ProviderId::LOCAL {
            return Ok((Vec::new(), None));
        }
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
        // Смешанный запрос — агрегат: локальные лайки плюс удачные
        // выдачи всех провайдеров, объединённые по TrackId. Сбой одного
        // провайдера не стирает ни локальные, ни чужие результаты.
        if provider.is_none() {
            return self.liked_aggregate().await;
        }
        // Явный провайдер — только его выдача, по-прежнему.
        // Fan-out параллельно по той же причине, что и `search`:
        // мультисессия не должна платить суммой латентностей.
        let targets = self.targets(provider)?;
        let results = futures_util::future::join_all(
            targets
                .iter()
                .map(|target| async move { (*target, target.catalog().liked().await) }),
        )
        .await;
        let mut out = Vec::new();
        for (target, result) in results {
            match result {
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

    /// Агрегат лайкнутого для `Cmd::Liked { provider: None }`:
    /// локально отмеченные (по свежести отметки) идут первыми, затем
    /// провайдерские в порядке Registry и выдачи провайдера. Свежие
    /// метаданные провайдера пишутся в кэш до чтения локальных строк,
    /// поэтому локальные дубли приходят с теми же свежими полями.
    async fn liked_aggregate(&self) -> anyhow::Result<Vec<Track>> {
        let targets = self.targets(None)?;
        let results = futures_util::future::join_all(
            targets
                .iter()
                .map(|target| async move { (*target, target.catalog().liked().await) }),
        )
        .await;
        let mut provider_order: Vec<TrackId> = Vec::new();
        for (target, result) in results {
            match result {
                Ok(found) => {
                    if !found.is_empty() {
                        self.with_cache(|c| c.put_tracks(&found))?;
                        for track in found {
                            if provider_order.iter().all(|id| id != &track.id) {
                                provider_order.push(track.id);
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(provider = %target.id(), %err, "лайки не прочитались");
                    self.report_auth(target, &err);
                    if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
                        self.try_reauth(target).await;
                    }
                }
            }
        }
        let local = self.with_cache(|c| c.liked_tracks())?;
        let mut out: Vec<Track> = local.into_iter().map(|(track, _)| track).collect();
        for id in provider_order {
            if !out.iter().any(|track| track.id == id) {
                // Метаданные только что легли в кэш, строка обязана быть.
                if let Some(track) = self.with_cache(|c| c.track(&id))? {
                    out.push(track);
                }
            }
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
        // Fan-out параллельно (причины те же, что у `search`), но
        // со строгим инвариантом home: полная ошибка любого провайдера
        // роняет весь ответ (кэш пишется только при полном успехе).
        let targets = self.targets(provider)?;
        let results = futures_util::future::join_all(
            targets
                .iter()
                .map(|target| async move { (*target, target.catalog().home().await) }),
        )
        .await;
        let mut out = Vec::new();
        for (target, result) in results {
            match result {
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

    /// Поставить оценку треку: локально сразу, у провайдера — зеркало.
    ///
    /// Локальная оценка — источник истины: запись и событие уходят ДО
    /// сетевого вызова, и сбой провайдера локальное состояние НЕ
    /// откатывает (иначе панель расходилась бы с тем, что человек
    /// нажал). Ошибка зеркала честно возвращается вызывающему и
    /// остаётся в журнале, `Unsupported` — не сбой: провайдер оценок
    /// не ведёт, отметка живёт только локально.
    pub(crate) async fn rate_track(
        &self,
        id: &TrackId,
        rating: Rating,
    ) -> anyhow::Result<()> {
        self.with_cache(|c| c.set_rating(id, rating))?;
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
            // Лайк меняет плейлист лайкнутого у провайдера (после
            // зеркала) — TTL-кэш его состава обязан протухнуть независимо
            // от исхода зеркала.
            let liked_playlist = PlaylistId::new(id.provider, "LM");
            self.with_cache(|c| c.forget_playlist_sync(&liked_playlist))?;
        }
        // Локальные треки зеркалить некуда: отметка только локальная.
        let Some(provider) = self.registry.get(id.provider) else {
            return Ok(());
        };
        match provider.catalog().rate(id, rating).await {
            Ok(()) => Ok(()),
            Err(tmus_provider::ProviderError::Unsupported { .. }) => {
                tracing::debug!(provider = %provider.id(), "провайдер не умеет оценки — отметка только локальная");
                Ok(())
            }
            Err(err) => {
                tracing::warn!(provider = %provider.id(), %err, "зеркало оценки не прошло; локальная отметка сохранена");
                self.report_auth(provider, &err);
                if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
                    self.try_reauth(provider).await;
                }
                Err(err.into())
            }
        }
    }

    /// Адресат нативного создания плейлиста (только явные удалённые:
    /// None/"local" уходят в локальный плейлист раньше). Явное имя
    /// обязательно: если такой провайдер не подключён — ошибка.
    fn playlist_provider(
        &self,
        explicit: Option<&str>,
    ) -> anyhow::Result<&Arc<dyn tmus_provider::Provider>> {
        let Some(name) = explicit else {
            anyhow::bail!("нативному созданию нужен явный провайдер");
        };
        self.registry
            .get_by_str(name)
            .ok_or_else(|| anyhow::anyhow!("провайдер {name} не подключён"))
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

    /// Создать плейлист. None или "local" — плейлист приложения в кэше
    /// (смешанные провайдеры, всегда доступно); явное имя удалённого —
    /// нативный плейлист этого провайдера.
    pub(crate) async fn playlist_create(
        &self,
        title: &str,
        provider: Option<&str>,
    ) -> anyhow::Result<Playlist> {
        if provider.is_none() || provider == Some(ProviderId::LOCAL.as_str()) {
            let playlist = self.with_cache(|c| c.local_playlist_create(title))?;
            self.emit(Event::PlaylistsChanged);
            return Ok(playlist);
        }
        let provider = self.playlist_provider(provider)?;
        match provider.catalog().playlist_create(title).await {
            Ok(playlist) => {
                self.emit(Event::PlaylistsChanged);
                Ok(playlist)
            }
            Err(err) => Err(self.report_playlist_error(provider, err).await),
        }
    }

    /// Переименовать плейлист. Нативного переименования в trait
    /// Catalog нет, поэтому у удалённых плейлистов команда — честная
    /// ошибка, а не тихий нооп.
    pub(crate) async fn playlist_rename(
        &self,
        playlist: &PlaylistId,
        title: &str,
    ) -> anyhow::Result<()> {
        if playlist.provider != ProviderId::LOCAL {
            anyhow::bail!(
                "переименование нативного плейлиста {} не поддерживается",
                playlist.provider
            );
        }
        self.with_cache(|c| c.local_playlist_rename(playlist, title))?;
        self.emit(Event::PlaylistsChanged);
        Ok(())
    }

    /// Добавить трек в плейлист и разослать сигнал.
    pub(crate) async fn playlist_add(
        &self,
        playlist: &PlaylistId,
        track: &TrackId,
    ) -> anyhow::Result<()> {
        if playlist.provider == ProviderId::LOCAL {
            // Метаданные обязаны уже лежать в кэше: чтение локального
            // состава джойнит tracks, и непрочитаемая запись — мина.
            // Если трек нигде не встречался, просим сначала найти/сыграть.
            if self.with_cache(|c| c.track(track))?.is_none() {
                anyhow::bail!("метаданных {track} нет в кэше — сначала найдите или проиграйте трек");
            }
            self.with_cache(|c| c.local_playlist_add(playlist, track))?;
            self.emit(Event::PlaylistsChanged);
            return Ok(());
        }
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
        if playlist.provider == ProviderId::LOCAL {
            // Локально дубликаты независимы, по одному id снять их все
            // нельзя: удаляем первую позицию трека, для точечной —
            // `PlaylistRemoveAt`.
            let position = self
                .with_cache(|c| c.local_playlist_tracks(playlist))?
                .iter()
                .position(|t| &t.id == track)
                .ok_or_else(|| anyhow::anyhow!("трека {track} нет в плейлисте {playlist}"))?;
            return self.playlist_remove_at(playlist, position).await;
        }
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

    /// Убрать запись плейлиста по абсолютной позиции и разослать
    /// сигнал. Единственный способ трогать дубликаты по отдельности.
    /// Нативного позиционного удаления у провайдеров нет — только
    /// локальные плейлисты.
    pub(crate) async fn playlist_remove_at(
        &self,
        playlist: &PlaylistId,
        position: usize,
    ) -> anyhow::Result<()> {
        if playlist.provider != ProviderId::LOCAL {
            anyhow::bail!("позиционное удаление доступно только локальным плейлистам");
        }
        self.with_cache(|c| c.local_playlist_remove_at(playlist, position))?;
        self.emit(Event::PlaylistsChanged);
        Ok(())
    }

    /// Удалить плейлист и разослать сигнал.
    pub(crate) async fn playlist_delete(&self, playlist: &PlaylistId) -> anyhow::Result<()> {
        if playlist.provider == ProviderId::LOCAL {
            self.with_cache(|c| c.local_playlist_delete(playlist))?;
            self.emit(Event::PlaylistsChanged);
            return Ok(());
        }
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

/// Правило дефолта источника каталога: сохранённый источник жив, пока
/// он существует и валиден; при отсутствии/невалидности — «все
/// клиенты» (`provider: None`), потому что мультисессионный режим
/// делает переключение источника необязательным.
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
        _ => CatalogSource { provider: None },
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
    fn invalid_saved_provider_falls_back_to_all() {
        let saved = CatalogSource { provider: Some("spotify".into()) };
        assert_eq!(
            resolve_catalog_source(Some(&saved), vec!["ytmusic".into(), "soundcloud".into()]),
            CatalogSource { provider: None }
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
    fn no_saved_source_is_all() {
        assert_eq!(
            resolve_catalog_source(None, Vec::new()),
            CatalogSource { provider: None }
        );
        assert_eq!(
            resolve_catalog_source(None, vec!["soundcloud".into()]),
            CatalogSource { provider: None }
        );
    }
}
