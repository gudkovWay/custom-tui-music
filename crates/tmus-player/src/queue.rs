//! Смешанная очередь воспроизведения.
//!
//! Очередь обязана оставаться провайдер-независимой: в ней одновременно
//! лежат треки разных `ProviderId`, и ничто в обходе не имеет права
//! смотреть на провайдера — это критерий приёмки абстракции, а не
//! приятная возможность.
//!
//! Shuffle реализован как перестановка ПОРЯДКА ОБХОДА, а не
//! перемешивание самого вектора: при выключении shuffle порядок обязан
//! вернуться к исходному, и перемешивание на месте это ломает.

use std::time::{SystemTime, UNIX_EPOCH};

use tmus_core::model::{LoopMode, Track, TrackId};

/// Детерминированный xorshift64. Зависимости `rand` в манифесте нет, а
/// для перемешивания очереди криптографическая случайность не нужна.
struct XorShift(u64);

impl XorShift {
    fn from_time() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        Self::new(nanos)
    }

    fn new(seed: u64) -> Self {
        // Нулевое состояние у xorshift мёртвое — всегда выдаёт ноль.
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Fisher–Yates поверх вектора индексов.
    fn shuffle_order(&mut self, len: usize) -> Vec<usize> {
        let mut order: Vec<usize> = (0..len).collect();
        for i in (1..len).rev() {
            let j = (self.next_u64() % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
        order
    }
}

pub struct Queue {
    tracks: Vec<Track>,
    /// Позиция текущего трека в порядке обхода (при shuffle это позиция
    /// в перестановке, а не индекс в `tracks`).
    cursor: Option<usize>,
    loop_mode: LoopMode,
    shuffle: bool,
    /// Актуальна только при `shuffle == true`.
    order: Vec<usize>,
}

impl Queue {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tracks: Vec::new(),
            cursor: None,
            loop_mode: LoopMode::None,
            shuffle: false,
            order: Vec::new(),
        }
    }

    pub fn append(&mut self, track: Track) {
        let index = self.tracks.len();
        self.tracks.push(track);
        if self.shuffle {
            self.order.push(index);
        }
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.order.clear();
        self.cursor = None;
    }

    /// Перейти к треку по индексу в исходном порядке. За границами —
    /// тихий no-op: команда из control-протокола не должна уметь
    /// ронять плеер.
    pub fn goto(&mut self, index: usize) -> bool {
        if index >= self.tracks.len() {
            return false;
        }
        self.cursor = Some(if self.shuffle {
            self.order
                .iter()
                .position(|&i| i == index)
                .unwrap_or(index)
        } else {
            index
        });
        true
    }

    #[must_use]
    pub fn current(&self) -> Option<&Track> {
        let cursor = self.cursor?;
        self.track_at_cursor(cursor)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    #[must_use]
    pub fn loop_mode(&self) -> LoopMode {
        self.loop_mode
    }

    pub fn set_loop_mode(&mut self, mode: LoopMode) {
        self.loop_mode = mode;
    }

    #[must_use]
    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    /// Включить/выключить shuffle. Текущий трек остаётся текущим.
    pub fn set_shuffle(&mut self, shuffle: bool) {
        if shuffle == self.shuffle {
            return;
        }
        // Текущий трек вычисляем ДО переключения режима: cursor хранит
        // позицию в порядке обхода, и она меняет смысл на границе.
        let current = if self.shuffle {
            self.cursor.and_then(|c| self.order.get(c).copied())
        } else {
            self.cursor
        };
        self.shuffle = shuffle;
        if shuffle {
            let mut rng = XorShift::from_time();
            self.order = rng.shuffle_order(self.tracks.len());
        } else {
            self.order.clear();
        }
        // Держим играющий трек на месте: при включении курсор маппится
        // в позицию трека в новой перестановке, при выключении это
        // просто индекс в векторе.
        self.cursor = match current {
            Some(index) if shuffle => self.order.iter().position(|&i| i == index),
            current => current,
        };
    }

    /// Индекс в `tracks` текущего трека.
    #[must_use]
    pub fn current_index(&self) -> Option<usize> {
        let cursor = self.cursor?;
        if self.shuffle {
            self.order.get(cursor).copied()
        } else {
            Some(cursor)
        }
    }

    /// Следующий трек без сдвига курсора — для предзагрузки.
    ///
    /// Предзагрузка — часть естественного продвижения: при repeat-track
    /// следующий «логически» — тот же трек, и готовить надо его
    /// (обновлять протухший URL), а не соседний.
    #[must_use]
    pub fn peek_next(&self) -> Option<&Track> {
        let cursor = self.advance_from(self.cursor?, 1, true)?;
        self.track_at_cursor(cursor)
    }

    /// Следующие `depth` треков без сдвига курсора — для фоновой докачки
    /// в офлайн-кэш. Глубина 1 — это `peek_next`; глубже — прогноз
    /// последовательного слушателя: замер 18.09 показал 8 мс старта
    /// закэшированного трека против ~5 с yt-dlp у незакэшированного,
    /// и пара скипов подряд не должна натыкаться на сеть.
    #[must_use]
    pub fn peek_ahead(&self, depth: usize) -> Vec<Track> {
        let mut out = Vec::with_capacity(depth);
        let mut cursor = match self.cursor {
            Some(cursor) => cursor,
            None => return out,
        };
        for _ in 0..depth {
            let Some(next) = self.advance_from(cursor, 1, true) else {
                break;
            };
            cursor = next;
            if let Some(track) = self.track_at_cursor(cursor) {
                out.push(track.clone());
            }
        }
        out
    }

    /// Ручная навигация (скип кнопкой/биндом): `LoopMode::Track`
    /// игнорируется, трек меняется всегда — иначе next на последнем
    /// треке залипал бы на месте.
    pub fn next(&mut self) -> Option<&Track> {
        let cursor = match self.cursor {
            None if !self.tracks.is_empty() => 0,
            Some(cursor) => match self.advance_from(cursor, 1, false) {
                Some(next) => next,
                None => return None,
            },
            None => return None,
        };
        self.cursor = Some(cursor);
        self.track_at_cursor(cursor)
    }

    /// Естественное продвижение (конец трека, предзагрузка): при
    /// `LoopMode::Track` остаёмся на текущем треке в ЛЮБОМ месте
    /// очереди, а не только на границе.
    pub fn next_natural(&mut self) -> Option<&Track> {
        let cursor = match self.cursor {
            None if !self.tracks.is_empty() => 0,
            Some(cursor) => self.advance_from(cursor, 1, true)?,
            None => return None,
        };
        self.cursor = Some(cursor);
        self.track_at_cursor(cursor)
    }

    /// Ручная навигация назад: `LoopMode::Track` игнорируется, на
    /// первом треке — wrap к последнему (симметрично `next`).
    pub fn prev(&mut self) -> Option<&Track> {
        let cursor = match self.cursor {
            None if !self.tracks.is_empty() => 0,
            Some(cursor) => match self.advance_from(cursor, -1, false) {
                Some(prev) => prev,
                None => return None,
            },
            None => return None,
        };
        self.cursor = Some(cursor);
        self.track_at_cursor(cursor)
    }

    /// Сдвиг курсора с учётом loop-режима. `delta` = ±1.
    ///
    /// `natural == true` — естественное продвижение (конец трека,
    /// предзагрузка, кэш-филлер): при `LoopMode::Track` повтор
    /// срабатывает в любом месте очереди. `natural == false` — ручная
    /// навигация: `LoopMode::Track` полностью игнорируется, на границе
    /// ведёт себя как `LoopMode::Queue` (wrap), при `LoopMode::None` —
    /// `None`.
    fn advance_from(&self, cursor: usize, delta: i64, natural: bool) -> Option<usize> {
        let len = self.order_len();
        debug_assert!(len > 0);
        if natural && self.loop_mode == LoopMode::Track {
            return Some(cursor);
        }
        let next = cursor as i64 + delta;
        if (0..len as i64).contains(&next) {
            return Some(next as usize);
        }
        match self.loop_mode {
            LoopMode::Track | LoopMode::Queue => {
                if delta > 0 { Some(0) } else { Some(len - 1) }
            }
            LoopMode::None => None,
        }
    }

    fn order_len(&self) -> usize {
        if self.shuffle { self.order.len() } else { self.tracks.len() }
    }

    fn track_at_cursor(&self, cursor: usize) -> Option<&Track> {
        let index = if self.shuffle {
            *self.order.get(cursor)?
        } else {
            cursor
        };
        self.tracks.get(index)
    }

    /// Позиция трека в очереди по его идентификатору.
    #[must_use]
    pub fn find_index(&self, id: &TrackId) -> Option<usize> {
        self.tracks.iter().position(|t| &t.id == id)
    }

    #[must_use]
    pub fn track_at(&self, index: usize) -> Option<&Track> {
        self.tracks.get(index)
    }

    /// Полный срез треков в исходном порядке — для персиста очереди.
    #[must_use]
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Восстановление из персиста: треки, позиция, режимы. Порядок обхода
    /// при shuffle пересобирается через set_shuffle — курсор остаётся на месте.
    pub fn restore(&mut self, tracks: Vec<Track>, index: Option<usize>, loop_mode: LoopMode, shuffle: bool) {
        self.tracks = tracks;
        self.order.clear();
        self.cursor = None;
        self.loop_mode = loop_mode;
        if shuffle {
            self.set_shuffle(true);
        }
        if let Some(i) = index {
            self.goto(i);
        }
    }
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmus_core::model::ProviderId;

    fn track(id: &str) -> Track {
        let (provider, rest) = id.split_once(':').expect("id в формате provider:track");
        let provider = match provider {
            "ytmusic" => ProviderId::YTMUSIC,
            "soundcloud" => ProviderId::SOUNDCLOUD,
            // ProviderId требует 'static: для тестов хватит статических
            // имён, остальные трактую как ytmusic.
            _ => ProviderId::YTMUSIC,
        };
        Track {
            id: TrackId::new(provider, rest),
            title: id.to_owned(),
            artists: Vec::new(),
            album: None,
            duration: None,
            art_url: None,
            page_url: None,
        }
    }

    fn queue(ids: &[&str]) -> Queue {
        let mut q = Queue::new();
        for id in ids {
            q.append(track(id));
        }
        q
    }

    fn assert_next(q: &mut Queue, expected: &str) {
        let got = q.next().expect("трек ожидался");
        assert_eq!(got.id.id, expected);
    }

    fn assert_next_natural(q: &mut Queue, expected: &str) {
        let got = q.next_natural().expect("трек ожидался");
        assert_eq!(got.id.id, expected);
    }

    #[test]
    fn next_without_loop_stops_at_end() {
        let mut q = queue(&["ytmusic:a", "ytmusic:b", "soundcloud:c"]);
        q.goto(0);
        assert_next(&mut q, "b");
        assert_next(&mut q, "c");
        // Конец очереди при LoopMode::None → None, курсор не слетает.
        assert!(q.next().is_none());
        assert_eq!(q.current_index(), Some(2));
    }

    #[test]
    fn next_with_queue_loop_wraps_around() {
        let mut q = queue(&["ytmusic:a", "soundcloud:b"]);
        q.set_loop_mode(LoopMode::Queue);
        q.goto(1);
        assert_next(&mut q, "a");
        assert_next(&mut q, "b");
    }

    #[test]
    fn next_with_track_loop_stays_put() {
        // Естественный конец трека идёт через next_natural: repeat-track
        // обязан работать в ЛЮБОМ месте очереди, а не только на границе.
        let mut q = queue(&["ytmusic:a", "ytmusic:b"]);
        q.set_loop_mode(LoopMode::Track);
        q.goto(1);
        assert_next_natural(&mut q, "b");
        assert_next_natural(&mut q, "b");
        assert_eq!(q.current_index(), Some(1));
    }

    #[test]
    fn natural_advance_mid_queue_with_track_loop_repeats_current() {
        // Главный кейс бага: трек кончился в середине очереди — repeat
        // track должен повторить его, а не утащить на следующий.
        let mut q = queue(&["ytmusic:a", "ytmusic:b", "ytmusic:c"]);
        q.set_loop_mode(LoopMode::Track);
        q.goto(1);
        assert_next_natural(&mut q, "b");
        assert_eq!(q.current_index(), Some(1));
    }

    #[test]
    fn manual_next_mid_queue_with_track_loop_advances() {
        // Ручной скип при repeat-track обязан менять трек: человек
        // нажал next — он хочет следующий, а не тот же.
        let mut q = queue(&["ytmusic:a", "ytmusic:b", "ytmusic:c"]);
        q.set_loop_mode(LoopMode::Track);
        q.goto(0);
        assert_next(&mut q, "b");
        assert_eq!(q.current_index(), Some(1));
    }

    #[test]
    fn manual_next_on_last_track_with_track_loop_wraps() {
        // Регресс залипания: раньше next на последнем треке при
        // LoopMode::Track возвращал тот же трек. Ручной скип — wrap.
        let mut q = queue(&["ytmusic:a", "ytmusic:b"]);
        q.set_loop_mode(LoopMode::Track);
        q.goto(1);
        assert_next(&mut q, "a");
        assert_eq!(q.current_index(), Some(0));
    }

    #[test]
    fn manual_prev_on_first_track_with_track_loop_wraps_to_last() {
        // Симметрия с next: ручной prev на первом треке — к последнему.
        let mut q = queue(&["ytmusic:a", "ytmusic:b"]);
        q.set_loop_mode(LoopMode::Track);
        q.goto(0);
        let got = q.prev().expect("трек ожидался");
        assert_eq!(got.id.id, "b");
        assert_eq!(q.current_index(), Some(1));
    }

    #[test]
    fn peek_next_mid_queue_with_track_loop_returns_current() {
        // Предзагрузка — часть естественного продвижения: при repeat
        // track готовить надо тот же трек (обновление протухшего URL).
        let mut q = queue(&["ytmusic:a", "ytmusic:b", "ytmusic:c"]);
        q.set_loop_mode(LoopMode::Track);
        q.goto(1);
        assert_eq!(q.peek_next().expect("трек ожидался").id.id, "b");
    }

    #[test]
    fn prev_moves_back() {
        let mut q = queue(&["ytmusic:a", "ytmusic:b"]);
        q.goto(1);
        let got = q.prev().expect("трек ожидался");
        assert_eq!(got.id.id, "a");
    }

    #[test]
    fn disabling_shuffle_restores_original_order() {
        // Регресс: перемешивание на месте делало выключение shuffle
        // необратимым. Здесь порядок обхода обязан вернуться к исходному.
        let mut q = queue(&["ytmusic:a", "ytmusic:b", "ytmusic:c", "ytmusic:d", "ytmusic:e"]);

        q.set_shuffle(true);
        // Обход с начала обхода: goto на первый элемент перестановки.
        q.goto(q.order[0]);
        // Курсор стоит на текущем треке: обход = текущий + все next.
        let mut shuffled: Vec<String> = vec![q.current().expect("текущий").id.id.clone()];
        while let Some(t) = q.next() {
            shuffled.push(t.id.id.clone());
        }
        assert_eq!(shuffled.len(), 5, "shuffle обязан обойти все треки");
        let mut seen = shuffled.clone();
        seen.sort();
        assert_eq!(seen, ["a", "b", "c", "d", "e"], "обход обязан быть перестановкой, без потерь и повторов");
        // Проверять здесь «порядок отличается от исходного» нельзя:
        // перестановка берётся от времени, тождественная выпадает с
        // вероятностью 1/120 — тест падал примерно раз на двадцать
        // прогонов. Что перемешивание действительно перемешивает,
        // проверяет детерминированный `shuffle_order_with_fixed_seed`.

        q.set_shuffle(false);
        q.goto(0);
        let mut restored: Vec<String> = vec![q.current().expect("текущий").id.id.clone()];
        while let Some(t) = q.next() {
            restored.push(t.id.id.clone());
        }
        assert_eq!(restored, ["a", "b", "c", "d", "e"]);
    }

    /// Перемешивание проверяется на фиксированном сиде: от времени оно
    /// иногда выдаёт тождественную перестановку, и тест на «порядок
    /// изменился» был бы флейки.
    #[test]
    fn shuffle_order_with_fixed_seed() {
        let order = XorShift::new(0x1234_5678_9ABC_DEF0).shuffle_order(5);
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, [0, 1, 2, 3, 4], "перестановка обязана быть без потерь и повторов");
        assert_ne!(order, [0, 1, 2, 3, 4], "на этом сиде порядок обязан отличаться от исходного");
    }

    #[test]
    fn shuffle_keeps_current_track() {
        let mut q = queue(&["ytmusic:a", "ytmusic:b", "ytmusic:c", "ytmusic:d"]);
        q.goto(2);
        q.set_shuffle(true);
        assert_eq!(q.current().expect("текущий трек").id.id, "c");
        q.set_shuffle(false);
        assert_eq!(q.current().expect("текущий трек").id.id, "c");
    }

    #[test]
    fn mixed_queue_traversal_is_provider_blind() {
        // Критерий приёмки абстракции: треки разных провайдеров лежат
        // рядом, обход их не различает.
        let mut q = queue(&["ytmusic:one", "soundcloud:two", "ytmusic:three"]);
        q.goto(0);
        assert_next(&mut q, "two");
        assert_next(&mut q, "three");
        assert!(q.next().is_none());

        q.set_loop_mode(LoopMode::Queue);
        assert_next(&mut q, "one");
    }

    #[test]
    fn goto_out_of_bounds_is_noop() {
        let mut q = queue(&["ytmusic:a"]);
        assert!(!q.goto(5));
        assert!(!q.goto(usize::MAX));
        assert!(q.current().is_none(), "курсор не должен был сдвинуться");
        assert!(q.goto(0));
        assert_eq!(q.current().expect("трек").id.id, "a");
    }

    #[test]
    fn goto_within_bounds_moves_cursor() {
        let mut q = queue(&["ytmusic:a", "ytmusic:b"]);
        assert!(q.goto(1));
        assert_eq!(q.current().expect("трек").id.id, "b");
    }

    #[test]
    fn next_on_empty_queue_is_none() {
        let mut q = Queue::new();
        assert!(q.next().is_none());
        assert!(q.prev().is_none());
        assert_eq!(q.len(), 0);
        assert!(q.is_empty());
    }

    #[test]
    fn shuffle_with_queue_loop_repeats_same_cycle_order() {
        let mut q = queue(&["ytmusic:a", "ytmusic:b", "ytmusic:c", "ytmusic:d", "ytmusic:e"]);
        q.set_loop_mode(LoopMode::Queue);
        q.set_shuffle(true);
        q.goto(q.order[0]);

        let mut first_cycle: Vec<String> = vec![q.current().expect("текущий").id.id.clone()];
        for _ in 1..q.len() {
            first_cycle.push(q.next().expect("трек").id.id.clone());
        }
        assert_eq!(first_cycle.len(), 5);
        let mut second_cycle: Vec<String> = Vec::new();
        for _ in 0..q.len() {
            second_cycle.push(q.next().expect("трек").id.id.clone());
        }
        assert_eq!(second_cycle, first_cycle, "второй цикл обязан повторить порядок первого");
    }

    #[test]
    fn restore_without_shuffle_keeps_index_and_modes() {
        let tracks = vec![
            track("ytmusic:a"),
            track("ytmusic:b"),
            track("ytmusic:c"),
            track("ytmusic:d"),
            track("ytmusic:e"),
        ];
        let mut q = Queue::new();
        q.restore(tracks.clone(), Some(2), LoopMode::Queue, false);
        assert_eq!(q.current_index(), Some(2));
        assert_eq!(q.current().expect("текущий трек").id.id, "c");
        assert_eq!(q.loop_mode(), LoopMode::Queue);
        assert_eq!(q.len(), 5);
        assert_eq!(q.tracks(), tracks.as_slice());
        assert!(!q.shuffle());
    }

    #[test]
    fn restore_with_shuffle_keeps_current_track() {
        // Shuffle детерминировать нельзя, но текущий трек обязан
        // остаться тем, что был под сохранённым индексом: порядок обхода
        // пересобирается, а goto маппит индекс в новую перестановку.
        let tracks = vec![
            track("ytmusic:a"),
            track("ytmusic:b"),
            track("ytmusic:c"),
            track("ytmusic:d"),
            track("ytmusic:e"),
        ];
        let mut q = Queue::new();
        q.restore(tracks, Some(2), LoopMode::None, true);
        assert!(q.shuffle());
        assert_eq!(q.current_index(), Some(2));
        assert_eq!(q.current().expect("текущий трек").id.id, "c");
    }

    #[test]
    fn append_after_shuffle_enabled_extends_traversal() {
        let mut q = queue(&["ytmusic:a", "ytmusic:b"]);
        q.set_shuffle(true);
        q.append(track("ytmusic:c"));
        // Курсор None → next начинает с начала порядка обхода.
        let mut count = 0;
        while q.next().is_some() {
            count += 1;
            if count > 10 {
                panic!("обход зациклился или не покрыл очередь");
            }
        }
        assert_eq!(count, 3, "ожидались все 3 трека");
    }
}
