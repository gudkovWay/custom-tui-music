use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tmus_core::model::{
    CatalogShelf, HomePage, Playlist, PlaylistId, ProviderId, Rating, SearchKind,
    SearchResult, Track, TrackId,
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
///
/// Курсоры продолжений здесь же: демон выдаёт клиенту собственный
/// непрозрачный идентификатор (`h0`, `h1`, …) и помнит, какому
/// провайдеру и какому сырому токену сервиса он соответствует. Сырые
/// токены агрегата наружу не утекают — по ним нельзя понять, чья это
/// страница, а клиенту и не нужно: токен провайдера без провайдера
/// бессмыслен.
pub(crate) struct HomeCache {
    key: String,
    at: Instant,
    page: HomePage,
    cursors: HashMap<String, (String, String)>,
    next_cursor: u64,
}

/// Слияние страниц домашней ленты: полки с одинаковым заголовком и
/// подзаголовком склеиваются, карточки-дубликаты (по каноническому id
/// результата) выбрасываются. Порядок страниц на входе — порядок
/// Registry, поэтому слияние детерминировано.
fn merge_home_pages(pages: Vec<HomePage>) -> HomePage {
    let mut shelves: Vec<CatalogShelf> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut next: Option<String> = None;
    for page in pages {
        if next.is_none() {
            next = page.next;
        }
        for shelf in page.shelves {
            let head = (shelf.title.clone(), shelf.subtitle.clone());
            let items = shelf
                .items
                .into_iter()
                .filter(|item| seen.insert(search_result_key(item)))
                .collect::<Vec<_>>();
            match shelves.iter_mut().find(|existing| {
                (existing.title.clone(), existing.subtitle.clone()) == head
            }) {
                Some(existing) => existing.items.extend(items),
                None => shelves.push(CatalogShelf { title: head.0, subtitle: head.1, items }),
            }
        }
    }
    HomePage { shelves, next }
}

/// Канонический ключ карточки полки: вид результата плюс составной id.
fn search_result_key(result: &SearchResult) -> String {
    match result {
        SearchResult::Track(track) => format!("t:{}", track.id),
        SearchResult::Playlist(playlist) => format!("p:{}", playlist.id),
        SearchResult::Artist { provider, id, .. } => format!("a:{}:{id}", provider.as_str()),
    }
}

/// Активная радио-сессия. Демон владеет ею целиком: провайдер сида,
/// сам сид, курсор догрузки и два флага — «дочитано» (курсор кончился
/// или провайдер отказал; повторов больше не будет) и «догрузка уже
/// идёт» (вторая параллельная не стартует).
pub(crate) struct RadioSession {
    provider: String,
    seed: TrackId,
    /// Номер поколения сессии (счётчик `App::radio_generation`):
    /// опоздавший сетевой ответ отличает свою сессию от сменившей её.
    generation: u64,
    continuation: Option<String>,
    exhausted: bool,
    refill_in_flight: bool,
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

    /// Первая страница домашней ленты с TTL-кэшем и single-flight.
    ///
    /// Замок `tokio::sync::Mutex` держится через весь сетевой заход:
    /// параллельные `Cmd::Home` сливаются в один, а не дёргают
    /// InnerTube горсткой. Протухшие полки не отдаём — сервис хранит
    /// прошлую ленту у себя, честная ошибка лучше несвежих советов.
    ///
    /// Курсоры продолжений агрегата — собственные идентификаторы
    /// демона (см. [`HomeCache`]); свежая страница перевыпускает их,
    /// и старые курсоры честно перестают существовать.
    pub(crate) async fn home(&self, provider: Option<&str>) -> anyhow::Result<HomePage> {
        let key = provider.unwrap_or("all").to_owned();
        let mut guard = self.home_cache.lock().await;
        if let Some(cached) = guard.as_ref() {
            if cached.key == key && cached.at.elapsed().as_secs() < HOME_TTL_SECS {
                return Ok(cached.page.clone());
            }
        }
        // Fan-out параллельно (причины те же, что у `search`), но
        // со строгим инвариантом home: полная ошибка любого провайдера
        // роняет весь ответ (кэш пишется только при полном успехе).
        // Страница едет вместе с id своего провайдера: токен
        // продолжения обязан остаться приписанным своему хозяину.
        let targets = self.targets(provider)?;
        let results = futures_util::future::join_all(
            targets.iter().map(|target| async move {
                let id = target.id().as_str().to_owned();
                (id, target.catalog().home_page(None).await)
            }),
        )
        .await;
        let mut pages = Vec::new();
        let mut continuation: Option<(String, String)> = None;
        for (id, result) in results {
            match result {
                Ok(page) => {
                    // Первый в порядке Registry токен и становится
                    // продолжением агрегата: детерминированно и ровно
                    // один — провайдеров с курсорами может быть несколько.
                    let mut page = page;
                    if continuation.is_none() {
                        if let Some(raw) = page.next.take() {
                            continuation = Some((id.clone(), raw));
                        }
                    }
                    pages.push(page);
                }
                Err(tmus_provider::ProviderError::Unsupported { .. }) => {
                    // Не каждый провайдер умеет домашнюю ленту: это не
                    // сбой, просто полки у него нет.
                    tracing::debug!(provider = %id, "домашней ленты нет");
                }
                Err(err) => {
                    tracing::warn!(provider = %id, %err, "домашняя лента не прочиталась");
                    let target = self.targets(Some(&id))?.remove(0);
                    self.report_auth(target, &err);
                    if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
                        self.try_reauth(target).await;
                    }
                    // Без stale-фолбэка: ошибки уходят клиенту целиком.
                    return Err(err.into());
                }
            }
        }
        let mut page = merge_home_pages(pages);
        let cards: Vec<SearchResult> =
            page.shelves.iter().flat_map(|s| s.items.iter().cloned()).collect();
        self.remember(&cards)?;
        // Курсоры продолжений перевыпускаются: нумерация продолжается
        // поверх прошлой, чтобы курсор прошлой ленты не совпал с новым.
        let mut cursors = HashMap::new();
        let mut next_cursor = guard.as_ref().map_or(0, |c| c.next_cursor);
        if let Some((owner, raw)) = continuation {
            let id = format!("h{next_cursor}");
            next_cursor += 1;
            // Курсор указывает на реального провайдера страницы, а не на
            // ключ разреза агрегата («all» провайдером не является).
            cursors.insert(id.clone(), (owner, raw));
            page.next = Some(id);
        }
        *guard = Some(HomeCache { key, at: Instant::now(), page: page.clone(), cursors, next_cursor });
        Ok(page)
    }

    /// Страница продолжения домашней ленты. Курсор — собственный
    /// идентификатор демона: он указывает и на провайдера, и на сырой
    /// токен сервиса. Ответ — обновлённая лента целиком (слияние и
    /// дедупликация на стороне демона), с курсором следующей страницы,
    /// если она есть.
    pub(crate) async fn home_more(
        &self,
        provider: Option<&str>,
        cursor: &str,
    ) -> anyhow::Result<HomePage> {
        let mut guard = self.home_cache.lock().await;
        let Some(cache) = guard.as_mut() else {
            anyhow::bail!("курсор {cursor} протух — запросите домашнюю ленту заново");
        };
        let Some((owner, raw)) = cache.cursors.get(cursor) else {
            anyhow::bail!("курсор {cursor} протух — запросите домашнюю ленту заново");
        };
        if let Some(asked) = provider {
            if asked != owner {
                anyhow::bail!("курсор {cursor} принадлежит провайдеру {owner}, а не {asked}");
            }
        }
        let owner = owner.clone();
        let raw = raw.clone();
        let target = self.targets(Some(&owner))?.remove(0);
        let page = match target.catalog().home_page(Some(&raw)).await {
            Ok(page) => page,
            Err(tmus_provider::ProviderError::Unsupported { .. }) => {
                // Провайдер выдал токен сам — «нет продолжения» значит
                // «дочитано», а не сбой.
                tracing::debug!(provider = %target.id(), "у домашней ленты нет продолжения");
                HomePage { shelves: Vec::new(), next: None }
            }
            Err(err) => {
                // Сбой сети — не смерть курсора: отображение и страница
                // остаются как были, повторный home_more с тем же hN
                // валиден. Списывание — только на успехе, ниже.
                tracing::warn!(provider = %target.id(), %err, "продолжение домашней ленты не прочиталось");
                self.report_auth(target, &err);
                if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
                    self.try_reauth(target).await;
                }
                return Err(err.into());
            }
        };
        // Страница приехала — старый курсор списан. Дальше в карту
        // вернётся только свежевыпущенный курсор, старый станет протухшим.
        cache.cursors.remove(cursor);
        // В кэше лежит непрозрачный курсор демона (hN), а не сырой токен
        // провайдера — в слияние он попасть не должен, иначе merge
        // «продлил» бы его как настоящий. Сливаем ленту с обнулённым
        // next: значимым токеном будет только свежий от провайдера.
        let mut previous = cache.page.clone();
        previous.next = None;
        let merged = merge_home_pages(vec![previous, page]);
        let cards: Vec<SearchResult> =
            merged.shelves.iter().flat_map(|s| s.items.iter().cloned()).collect();
        self.remember(&cards)?;
        let mut cursors = std::mem::take(&mut cache.cursors);
        let next_cursor = cache.next_cursor;
        let mut out = merged;
        if let Some(raw) = out.next.take() {
            let id = format!("h{next_cursor}");
            cursors.insert(id.clone(), (owner, raw));
            out.next = Some(id);
        }
        cache.page = out.clone();
        cache.next_cursor = next_cursor + if out.next.is_some() { 1 } else { 0 };
        cache.cursors = cursors;
        Ok(out)
    }

    // ────────────────────────────────────────── радио (автодополнение)

    /// На сколько позиций до хвоста очереди начинается догрузка радио:
    /// к моменту, когда человек дойдёт до рекомендаций, они уже должны
    /// стоять в очереди, но дёргать сеть за пять треков вперёд рано.
    const RADIO_REFILL_AHEAD: usize = 2;

    /// Запустить радио по сид-треку. Провайдер без радио (`Unsupported`)
    /// — не сбой: фолбэк в конечное воспроизведение одного трека, как
    /// раньше играл `Cmd::PlayTrack`.
    pub(crate) async fn play_radio(&self, seed: &TrackId) -> anyhow::Result<()> {
        let provider = self
            .registry
            .get(seed.provider)
            .ok_or_else(|| anyhow::anyhow!("провайдер {} не подключён", seed.provider))?;
        // Сид hydrate'ится до сессии: `?` здесь не должен оставлять
        // сессию в подвешенном in-flight.
        let seed_track = self.hydrate(std::slice::from_ref(seed))?.remove(0);
        // Поколение берётся до сетевого захода: ответ, приехавший после
        // явной замены контекста, узнаёт себя по номеру и уходит в
        // никуда, не трогая новую очередь.
        let generation = self.radio_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        // Сессия ставится до сетевого захода с in-flight флагом: параллельный
        // `Cmd::PlayRadio` или преждевременная догрузка не стартуют вторые.
        *self.radio.lock().await = Some(RadioSession {
            provider: seed.provider.as_str().to_owned(),
            seed: seed.clone(),
            generation,
            continuation: None,
            exhausted: false,
            refill_in_flight: true,
        });
        let first = match provider.catalog().radio(seed, None).await {
            Ok(page) => page,
            Err(tmus_provider::ProviderError::Unsupported { .. }) => {
                // Опоздавший запрос (сессию сменили, пока мы ждали) уходит
                // в никуда: ни сессию чужую не снимает, ни конечный фолбэк
                // не играет поверх чужой очереди.
                let mut radio = self.radio.lock().await;
                if !matches!(radio.as_ref(), Some(session) if session.generation == generation) {
                    return Ok(());
                }
                tracing::debug!(provider = %provider.id(), "радио не поддерживается — конечное воспроизведение");
                *radio = None;
                // Фолбэк под тем же замком: новый PlayRadio или явная
                // замена подождут за ним; inner замок радио не берёт.
                return self.play_track_inner(seed).await;
            }
            Err(err) => {
                // Ошибка первой страницы: сессию снимаем, только если она
                // всё ещё наша — чужую (более новую) не трогаем. Проверка
                // и снятие под одним захватом замка: окно между ними
                // позволило бы стереть вклинившуюся свежую сессию.
                {
                    let mut radio = self.radio.lock().await;
                    if matches!(radio.as_ref(), Some(session) if session.generation == generation)
                    {
                        *radio = None;
                    }
                }
                // Повторять курсор нечего: это была первая страница.
                tracing::warn!(provider = %provider.id(), %err, "радио не запустилось");
                self.report_auth(provider, &err);
                if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
                    self.try_reauth(provider).await;
                }
                return Err(err.into());
            }
        };
        // Сессия могла быть отменена или заменена, пока ехала первая
        // страница. Замок держим от проверки до конца финализации:
        // явная замена контекста подождёт нас за этим же замком и
        // только потом заменит очередь, а не наоборот.
        let mut radio = self.radio.lock().await;
        match radio.as_ref() {
            Some(session) if session.generation == generation => {}
            _ => return Ok(()),
        }
        // Очередь: сид первым, затем рекомендации без дубликатов сида
        // и между собой.
        let mut seen: Vec<TrackId> = vec![seed.clone()];
        let mut tracks = vec![seed_track];
        for track in first.tracks {
            if seen.contains(&track.id) {
                continue;
            }
            seen.push(track.id.clone());
            tracks.push(track);
        }
        // Метаданные принятых рекомендаций пишутся в кэш ДО попадания в
        // очередь: иначе «добавить в локальный плейлист» сразу после
        // старта радио упрётся в непрочитанную запись. Сюда доходят
        // только ответы текущего поколения — опоздавший ответ вышел
        // выше, чужую страницу в кэш не положит.
        if tracks.len() > 1 {
            if let Err(err) = self.with_cache(|c| c.put_tracks(&tracks[1..])) {
                // Кэш не принял страницу — треки не имеют права стать
                // видимыми (добавление в локальный плейлист упёрлось бы
                // в непрочитанную запись). Снимаем только свою сессию и
                // выходим с ошибкой ДО мутации очереди.
                *radio = None;
                return Err(err.into());
            }
        }
        let len = self
            .player
            .with_queue(|q| {
                q.clear();
                for track in &tracks {
                    q.append(track.clone());
                }
                q.goto(0);
                q.len()
            })
            .await;
        self.emit(Event::QueueChanged { len, index: Some(0) });
        // Собственная замена очереди себя не хоронит: отмена живёт в
        // явных заменах контекста, а не в мутации очереди.
        if let Some(session) = radio.as_mut() {
            session.continuation = first.next.clone();
            session.exhausted = first.next.is_none();
            session.refill_in_flight = false;
        }
        drop(radio);
        self.play_known(seed).await
    }

    /// Точка догрузки из вахтёра: по transitions плеера смотрим, не
    /// подошла ли очередь к хвосту, и в фоне дописываем рекомендации.
    /// Дешёвая проверка под замком; сетевой заход уходит в задачу.
    pub(crate) async fn maybe_refill_radio(self: &Arc<Self>, queue_len: usize, queue_index: Option<usize>) {
        let Some(index) = queue_index else { return };
        if index + Self::RADIO_REFILL_AHEAD < queue_len {
            return;
        }
        let job = {
            let mut radio = self.radio.lock().await;
            let Some(session) = radio.as_mut() else { return };
            // Нет сессии, всё дочитано, уже идёт догрузка или догружать
            // нечем — все четыре случая означают «сейчас ничего не делать»;
            // in-flight гасит и тугую петлю повторных вызовов вахтёра.
            if session.exhausted
                || session.refill_in_flight
                || session.continuation.is_none()
            {
                return;
            }
            let cursor = session.continuation.clone().expect("проверено выше");
            session.refill_in_flight = true;
            (session.generation, session.provider.clone(), session.seed.clone(), cursor)
        };
        tokio::spawn(self.clone().radio_refill(job));
    }

    /// Фоновая догрузка одной страницы радио. Любой сбой — исчерпание
    /// сессии: одна и та же страница не retried-ится, а успешная
    /// догрузка дописывает в хвост без замены очереди.
    async fn radio_refill(
        self: Arc<Self>,
        (generation, provider_name, seed, cursor): (u64, String, TrackId, String),
    ) {
        let Some(provider) = self.registry.get_by_str(&provider_name) else {
            // Провайдер отключился за время сети: сессию снимаем, только
            // если она всё ещё та, из которой мы ушли.
            let mut radio = self.radio.lock().await;
            if matches!(radio.as_ref(), Some(session) if session.generation == generation) {
                *radio = None;
            }
            return;
        };
        let page = match provider.catalog().radio(&seed, Some(&cursor)).await {
            Ok(page) => page,
            Err(tmus_provider::ProviderError::Unsupported { .. }) => {
                tracing::debug!(provider = %provider_name, "радио дочитано");
                let mut radio = self.radio.lock().await;
                if let Some(session) = radio.as_mut().filter(|s| s.generation == generation) {
                    session.exhausted = true;
                    session.refill_in_flight = false;
                }
                return;
            }
            Err(err) => {
                // Ограниченный сбой: курсор не повторяем, автодополнение
                // честно останавливается. Очередь продолжает играть.
                tracing::warn!(provider = %provider_name, %err, "догрузка радио не удалась — автодополнение остановлено");
                self.report_auth(provider, &err);
                if matches!(err, tmus_provider::ProviderError::Auth { .. }) {
                    self.try_reauth(provider).await;
                }
                let mut radio = self.radio.lock().await;
                if let Some(session) = radio.as_mut().filter(|s| s.generation == generation) {
                    session.exhausted = true;
                    session.refill_in_flight = false;
                }
                return;
            }
        };
        // Дедупликация против текущей очереди — в момент записи, а не
        // до сетевого захода: пока страница ехала, очередь могла
        // измениться. Под тем же замком проверяем, что сессия — та же
        // (курсор совпадает): за время сети человек мог запустить
        // другой контекст, и чужой хвост в новую очередь писать нельзя.
        let mut radio = self.radio.lock().await;
        let Some(session) = radio.as_mut() else { return };
        // Поколение — единственная защита от подмены сессии: два радио
        // по одному сиду различаются номерами, курсор же может совпасть.
        if session.generation != generation {
            return;
        }
        // Принятые треки фильтруются против текущей очереди в момент
        // записи (пока страница ехала, очередь могла измениться) и
        // против дубликатов внутри самой страницы. Кэш пишется ДО
        // появления треков в очереди; поколение проверено выше, чужая
        // страница сюда не доходит.
        let mut accepted: Vec<Track> = Vec::new();
        let mut page_seen: Vec<TrackId> = Vec::new();
        self.player
            .with_queue(|q| {
                for track in page.tracks {
                    if q.find_index(&track.id).is_some() || page_seen.contains(&track.id) {
                        continue;
                    }
                    page_seen.push(track.id.clone());
                    accepted.push(track);
                }
            })
            .await;
        if !accepted.is_empty() {
            if let Err(err) = self.with_cache(|c| c.put_tracks(&accepted)) {
                // Страница не закэшировалась — в очередь её не
                // дописываем: видимые треки обязаны быть
                // кэш-обеспеченными. Сессию помечаем исчерпанной и
                // гасим in-flight (без retry-цикла), только если она
                // всё ещё наша.
                tracing::warn!(%err, "кэширование страницы радио не удалось — автодополнение остановлено");
                if let Some(session) = radio.as_mut().filter(|s| s.generation == generation) {
                    session.exhausted = true;
                    session.refill_in_flight = false;
                }
                return;
            }
        }
        let (len, index) = self
            .player
            .with_queue(|q| {
                for track in accepted {
                    // После кэширования очередь могла измениться
                    // (QueueAppend): дубль-проверка — атомарно с
                    // вставкой, под тем же замком.
                    if q.find_index(&track.id).is_some() {
                        continue;
                    }
                    q.append(track);
                }
                (q.len(), q.current_index())
            })
            .await;
        self.emit(Event::QueueChanged { len, index });
        session.continuation = page.next.clone();
        session.exhausted = page.next.is_none();
        session.refill_in_flight = false;
    }

    /// Отмена радио-сессии: явная замена контекста (`PlayTrack`,
    /// `PlayPlaylist`, `PlayContext`, `QueueClear`) хоронит сессию,
    /// чтобы хвост старого радио не прирастал к новой очереди.
    pub(crate) async fn cancel_radio(&self) {
        *self.radio.lock().await = None;
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
