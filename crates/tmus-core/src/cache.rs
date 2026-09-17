//! Офлайн-кэш: метаданные в SQLite, скачанные аудиофайлы — на диске.
//!
//! Офлайн заказан явно, а не «допилим потом»: в дороге сети нет, и без
//! этого плеер бесполезен ровно там, где нужен. Отсюда две части в одном
//! модуле — база нужна и чтобы найти скачанное, и чтобы решить, что
//! выкидывать при переполнении.
//!
//! В каждой таблице первичный ключ составной `(provider, id)`. Это не
//! педантизм, а прямое следствие главного правила проекта: строковый id
//! не уникален между провайдерами, и `ytmusic:x` с `soundcloud:x` —
//! разные записи. С ключом только по `id` вторая молча перетёрла бы
//! первую, а в UI это выглядело бы как «плейлист подменился».
//!
//! Модуль синхронный: `rusqlite` блокирует поток, и делать вид, что это
//! не так, оборачивая его в `async`, — обманывать вызывающего. Где его
//! крутить, решает демон.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{CoreError, Result};
use crate::model::{Playlist, PlaylistId, ProviderId, Track, TrackId};
use crate::paths::Paths;
use crate::protocol::CacheStats;

/// Схема. Идемпотентна: `Cache::open` вызывается на каждом старте, а
/// миграций пока нет — перезапись живых данных была бы их потерей.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tracks (
    provider    TEXT    NOT NULL,
    id          TEXT    NOT NULL,
    title       TEXT    NOT NULL,
    -- JSON-массив: артистов бывает несколько, фиксированных колонок не
    -- хватит, и «feat.» уехал бы в title.
    artists     TEXT    NOT NULL,
    album       TEXT,
    duration_ms INTEGER,
    art_url     TEXT,
    page_url    TEXT,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (provider, id)
);

CREATE TABLE IF NOT EXISTS playlists (
    provider    TEXT    NOT NULL,
    id          TEXT    NOT NULL,
    title       TEXT    NOT NULL,
    subtitle    TEXT,
    art_url     TEXT,
    track_count INTEGER,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (provider, id)
);

-- Порядок в плейлисте значим, поэтому позиция входит в ключ, а не лежит
-- атрибутом: иначе восстановление очереди после рестарта перемешало бы
-- треки.
CREATE TABLE IF NOT EXISTS playlist_tracks (
    provider       TEXT    NOT NULL,
    playlist       TEXT    NOT NULL,
    position       INTEGER NOT NULL,
    track_provider TEXT    NOT NULL,
    track_id       TEXT    NOT NULL,
    PRIMARY KEY (provider, playlist, position)
);

-- Учёт офлайн-файлов. `path` и `bytes` пишутся из файловой системы, а не
-- из аргументов вызова: иначе учёт разъедется с диском, и gc будет
-- считать лимит по несуществующим байтам.
CREATE TABLE IF NOT EXISTS audio (
    provider    TEXT    NOT NULL,
    id          TEXT    NOT NULL,
    path        TEXT    NOT NULL,
    bytes       INTEGER NOT NULL,
    ext         TEXT,
    pinned      INTEGER NOT NULL DEFAULT 0,
    accessed_at INTEGER NOT NULL,
    PRIMARY KEY (provider, id)
);

-- gc ходит именно так: незакреплённые, самые старые первыми.
CREATE INDEX IF NOT EXISTS audio_lru ON audio (pinned, accessed_at);
";

/// Колонки трека в том порядке, в котором их читает [`track_from_row`].
/// Одна константа на все запросы: разъехавшийся порядок колонок — это
/// перепутанные артист с альбомом, а не ошибка компиляции.
const TRACK_COLUMNS: &str =
    "t.provider, t.id, t.title, t.artists, t.album, t.duration_ms, t.art_url, t.page_url";

const TRACK_UPSERT: &str = "
INSERT INTO tracks (provider, id, title, artists, album, duration_ms, art_url, page_url, updated_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
ON CONFLICT (provider, id) DO UPDATE SET
    title       = excluded.title,
    artists     = excluded.artists,
    album       = excluded.album,
    duration_ms = excluded.duration_ms,
    art_url     = excluded.art_url,
    page_url    = excluded.page_url,
    updated_at  = excluded.updated_at";

const PLAYLIST_COLUMNS: &str =
    "p.provider, p.id, p.title, p.subtitle, p.art_url, p.track_count";

const PLAYLIST_UPSERT: &str = "
INSERT INTO playlists (provider, id, title, subtitle, art_url, track_count, updated_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
ON CONFLICT (provider, id) DO UPDATE SET
    title       = excluded.title,
    subtitle    = excluded.subtitle,
    art_url     = excluded.art_url,
    track_count = excluded.track_count,
    updated_at  = excluded.updated_at";

const AUDIO_UPSERT: &str = "
INSERT INTO audio (provider, id, path, bytes, ext, pinned, accessed_at)
VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)
ON CONFLICT (provider, id) DO UPDATE SET
    path        = excluded.path,
    bytes       = excluded.bytes,
    ext         = excluded.ext,
    accessed_at = excluded.accessed_at";

/// Метаданные и учёт офлайн-файлов.
pub struct Cache {
    conn: Connection,
    paths: Paths,
    limit_bytes: u64,
}

impl Cache {
    /// Открыть (создать при необходимости) базу в `paths.database()`.
    ///
    /// `limit_bytes` приходит снаружи и здесь не проверяется: сколько
    /// места отдавать кэшу — решение конфига, а не базы.
    pub fn open(paths: &Paths, limit_bytes: u64) -> Result<Self> {
        let db = paths.database();
        // Каталоги создаём сами: на чистой машине их ещё нет, а падать
        // на первом запуске из-за отсутствия каталога незачем.
        if let Some(parent) = db.parent() {
            create_dir(parent)?;
        }
        create_dir(&paths.audio_dir())?;

        let conn = Connection::open(&db).map_err(|source| CoreError::Database {
            path: db.clone(),
            source,
        })?;
        // WAL: демон читает статистику, пока идёт запись метаданных; без
        // него читатель и писатель блокируют друг друга.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|source| CoreError::Database { path: db.clone(), source })?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|source| CoreError::Database { path: db.clone(), source })?;
        conn.execute_batch(SCHEMA)
            .map_err(|source| CoreError::Database { path: db, source })?;

        Ok(Self {
            conn,
            paths: paths.clone(),
            limit_bytes,
        })
    }

    /// Записать треки. Существующие записи обновляются: провайдер мог
    /// поправить название или длительность.
    pub fn put_tracks(&self, tracks: &[Track]) -> Result<()> {
        let updated_at = now_secs();
        let tx = self.transaction()?;
        {
            let mut upsert = tx.prepare(TRACK_UPSERT).map_err(|e| self.db_error(e))?;
            for track in tracks {
                self.upsert_track(&mut upsert, track, updated_at)?;
            }
        }
        tx.commit().map_err(|e| self.db_error(e))?;
        Ok(())
    }

    pub fn track(&self, id: &TrackId) -> Result<Option<Track>> {
        let sql =
            format!("SELECT {TRACK_COLUMNS} FROM tracks t WHERE t.provider = ?1 AND t.id = ?2");
        self.conn
            .query_row(
                &sql,
                params![id.provider.as_str(), id.id.as_str()],
                track_from_row,
            )
            .optional()
            .map_err(|e| self.db_error(e))
    }

    pub fn put_playlists(&self, playlists: &[Playlist]) -> Result<()> {
        let updated_at = now_secs();
        let tx = self.transaction()?;
        {
            let mut upsert = tx.prepare(PLAYLIST_UPSERT).map_err(|e| self.db_error(e))?;
            for playlist in playlists {
                upsert
                    .execute(params![
                        playlist.id.provider.as_str(),
                        playlist.id.id.as_str(),
                        playlist.title.as_str(),
                        playlist.subtitle.as_deref(),
                        playlist.art_url.as_deref(),
                        playlist.track_count.map(i64::from),
                        updated_at,
                    ])
                    .map_err(|e| self.db_error(e))?;
            }
        }
        tx.commit().map_err(|e| self.db_error(e))?;
        Ok(())
    }

    /// Плейлисты, при желании — только одного провайдера.
    pub fn playlists(&self, provider: Option<&str>) -> Result<Vec<Playlist>> {
        // `provider IS NULL` в условии вместо двух запросов: ветвление в
        // SQL дешевле, чем дублирование списка колонок, который обязан
        // совпадать с `playlist_from_row`.
        let sql = format!(
            "SELECT {PLAYLIST_COLUMNS} FROM playlists p
             WHERE ?1 IS NULL OR p.provider = ?1
             ORDER BY p.provider, p.title COLLATE NOCASE"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(|e| self.db_error(e))?;
        let rows = stmt
            .query_map(params![provider], playlist_from_row)
            .map_err(|e| self.db_error(e))?;
        collect(rows, |e| self.db_error(e))
    }

    /// Заменить состав плейлиста целиком, сохранив порядок.
    pub fn put_playlist_tracks(&self, playlist: &PlaylistId, tracks: &[Track]) -> Result<()> {
        let updated_at = now_secs();
        let tx = self.transaction()?;
        {
            // Сначала чистим: плейлист мог укоротиться, и хвост прежних
            // позиций иначе остался бы в очереди навсегда.
            tx.execute(
                "DELETE FROM playlist_tracks WHERE provider = ?1 AND playlist = ?2",
                params![playlist.provider.as_str(), playlist.id.as_str()],
            )
            .map_err(|e| self.db_error(e))?;

            let mut insert = tx
                .prepare(
                    "INSERT INTO playlist_tracks
                         (provider, playlist, position, track_provider, track_id)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(|e| self.db_error(e))?;
            let mut upsert = tx.prepare(TRACK_UPSERT).map_err(|e| self.db_error(e))?;
            for (position, track) in tracks.iter().enumerate() {
                // Треки кладём и в `tracks`: иначе плейлист читался бы
                // как набор id без названий, а в UI это пустые строки.
                self.upsert_track(&mut upsert, track, updated_at)?;
                insert
                    .execute(params![
                        playlist.provider.as_str(),
                        playlist.id.as_str(),
                        position as i64,
                        track.id.provider.as_str(),
                        track.id.id.as_str(),
                    ])
                    .map_err(|e| self.db_error(e))?;
            }
        }
        tx.commit().map_err(|e| self.db_error(e))?;
        Ok(())
    }

    /// Треки плейлиста в сохранённом порядке.
    pub fn playlist_tracks(&self, playlist: &PlaylistId) -> Result<Vec<Track>> {
        let sql = format!(
            "SELECT {TRACK_COLUMNS}
             FROM playlist_tracks pt
             JOIN tracks t ON t.provider = pt.track_provider AND t.id = pt.track_id
             WHERE pt.provider = ?1 AND pt.playlist = ?2
             ORDER BY pt.position"
        );
        let mut stmt = self.conn.prepare(&sql).map_err(|e| self.db_error(e))?;
        let rows = stmt
            .query_map(
                params![playlist.provider.as_str(), playlist.id.as_str()],
                track_from_row,
            )
            .map_err(|e| self.db_error(e))?;
        collect(rows, |e| self.db_error(e))
    }

    /// Куда класть скачанный файл.
    ///
    /// Имя санируется: провайдерский id — чужая строка, и `../../etc/passwd`
    /// без этого увёл бы запись за пределы кэша. Это защита, а не
    /// косметика.
    ///
    /// Плата за неё — теоретическое слияние двух разных id в одно имя
    /// (`a/b` и `a_b`): реальные id провайдеров алфавитно-цифровые, а
    /// выход за каталог кэша стоит дороже возможного повторного
    /// скачивания.
    #[must_use]
    pub fn audio_path(&self, id: &TrackId, ext: &str) -> PathBuf {
        let name = format!("{}.{}", sanitize(&id.id), sanitize(ext));
        self.paths.audio_dir_for(id.provider.as_str()).join(name)
    }

    /// Путь к скачанному файлу, если он есть и на записи, и на диске.
    ///
    /// Запись без файла удаляется: кэш мог быть подчищен снаружи
    /// (tmpfiles, ручной `rm`, чистка диска), и висячая запись врала бы
    /// и вызывающему, и учёту лимита.
    ///
    /// Попадание обновляет `accessed_at` — на этом держится LRU в
    /// [`Cache::gc`].
    pub fn lookup_audio(&self, id: &TrackId) -> Result<Option<PathBuf>> {
        let found: Option<String> = self
            .conn
            .query_row(
                "SELECT path FROM audio WHERE provider = ?1 AND id = ?2",
                params![id.provider.as_str(), id.id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| self.db_error(e))?;
        let Some(found) = found else {
            return Ok(None);
        };

        let path = PathBuf::from(found);
        if !path.is_file() {
            self.forget_audio(id.provider.as_str(), &id.id)?;
            return Ok(None);
        }

        self.conn
            .execute(
                "UPDATE audio SET accessed_at = ?3 WHERE provider = ?1 AND id = ?2",
                params![id.provider.as_str(), id.id.as_str(), now_secs()],
            )
            .map_err(|e| self.db_error(e))?;
        Ok(Some(path))
    }

    /// Взять файл на учёт. Размер читается из файловой системы, а не
    /// берётся параметром: переданное число рано или поздно разойдётся с
    /// диском (обрезка, чужая запись) и лимит поедет.
    pub fn register_audio(&self, id: &TrackId, path: &Path, ext: &str) -> Result<()> {
        let bytes = std::fs::metadata(path)
            .map_err(|source| CoreError::Cache {
                path: path.to_path_buf(),
                source,
            })?
            .len();
        // В базе путь лежит текстом; молча потерять не-UTF-8 путь нельзя —
        // тогда `lookup_audio` вернул бы другую строку, чем записали.
        let stored = path.to_str().ok_or_else(|| CoreError::Cache {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidData, "путь кэша не в UTF-8"),
        })?;

        self.conn
            .execute(
                AUDIO_UPSERT,
                params![
                    id.provider.as_str(),
                    id.id.as_str(),
                    stored,
                    i64::try_from(bytes).unwrap_or(i64::MAX),
                    ext,
                    now_secs(),
                ],
            )
            .map_err(|e| self.db_error(e))?;
        Ok(())
    }

    /// Закрепить или открепить треки. Закреплённое не вытесняется
    /// [`Cache::gc`] никогда: офлайн-кэш без этого съел бы диск и всё
    /// равно выбросил то, что нужно в дороге.
    ///
    /// Закреплять имеет смысл только уже взятые на учёт треки: у
    /// незарегистрированного файла нет ни пути, ни размера, и
    /// [`Cache::lookup_audio`] снёс бы такую запись при первой же
    /// проверке вместе с пинком.
    pub fn set_pinned(&self, ids: &[TrackId], pinned: bool) -> Result<()> {
        let tx = self.transaction()?;
        {
            let mut stmt = tx
                .prepare("UPDATE audio SET pinned = ?3 WHERE provider = ?1 AND id = ?2")
                .map_err(|e| self.db_error(e))?;
            for id in ids {
                stmt.execute(params![id.provider.as_str(), id.id.as_str(), pinned])
                    .map_err(|e| self.db_error(e))?;
            }
        }
        tx.commit().map_err(|e| self.db_error(e))?;
        Ok(())
    }

    /// `tracks`/`bytes` — офлайн-файлы, а не строки метаданных: рядом
    /// стоят `pinned_tracks`/`pinned_bytes`, и они обязаны быть их
    /// подмножеством, иначе цифры в UI не сходятся.
    pub fn stats(&self) -> Result<CacheStats> {
        let (tracks, bytes, pinned_tracks, pinned_bytes): (i64, i64, i64, i64) = self
            .conn
            .query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(bytes), 0),
                        COALESCE(SUM(pinned), 0),
                        COALESCE(SUM(CASE WHEN pinned = 1 THEN bytes ELSE 0 END), 0)
                 FROM audio",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|e| self.db_error(e))?;
        Ok(CacheStats {
            tracks: to_u64(tracks),
            bytes: to_u64(bytes),
            limit_bytes: self.limit_bytes,
            pinned_tracks: to_u64(pinned_tracks),
            pinned_bytes: to_u64(pinned_bytes),
        })
    }

    /// Вытеснить самое старое, пока незакреплённое не влезет в лимит.
    /// Возвращает освобождённые на диске байты.
    ///
    /// Решение о вытеснении принимается по учтённым размерам, а
    /// возвращаемое число — по факту: файла могло уже не быть, и
    /// рапортовать о несуществующих байтах нельзя.
    pub fn gc(&self) -> Result<u64> {
        let limit = i64::try_from(self.limit_bytes).unwrap_or(i64::MAX);
        let mut remaining: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(bytes), 0) FROM audio WHERE pinned = 0",
                [],
                |row| row.get(0),
            )
            .map_err(|e| self.db_error(e))?;
        if remaining <= limit {
            return Ok(0);
        }

        let mut stmt = self
            .conn
            .prepare(
                "SELECT provider, id, path, bytes FROM audio
                 WHERE pinned = 0
                 ORDER BY accessed_at, provider, id",
            )
            .map_err(|e| self.db_error(e))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Victim {
                    provider: row.get(0)?,
                    id: row.get(1)?,
                    path: PathBuf::from(row.get::<_, String>(2)?),
                    bytes: row.get(3)?,
                })
            })
            .map_err(|e| self.db_error(e))?;
        // Список кандидатов разворачиваем до удалений: удалять строки
        // из таблицы, которую прямо сейчас листает открытый курсор, —
        // полагаться на недокументированный порядок, а набор кандидатов
        // известен заранее и в процессе меняться не должен.
        let victims: Vec<Victim> = collect(rows, |e| self.db_error(e))?;
        drop(stmt);

        let mut freed = 0_u64;
        for victim in victims {
            if remaining <= limit {
                break;
            }
            match std::fs::metadata(&victim.path) {
                Ok(meta) => {
                    std::fs::remove_file(&victim.path).map_err(|source| CoreError::Cache {
                        path: victim.path.clone(),
                        source,
                    })?;
                    freed += meta.len();
                }
                // Файла нет — не ошибка, но запись всё равно уходит:
                // иначе она снова и снова попадала бы в кандидаты.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(CoreError::Cache {
                        path: victim.path,
                        source,
                    });
                }
            }
            self.forget_audio(&victim.provider, &victim.id)?;
            remaining -= victim.bytes;
        }
        Ok(freed)
    }

    fn upsert_track(
        &self,
        stmt: &mut rusqlite::Statement<'_>,
        track: &Track,
        updated_at: i64,
    ) -> Result<()> {
        let artists = serde_json::to_string(&track.artists).map_err(|e| {
            // `Vec<String>` сериализуется без ошибок, но паниковать в
            // библиотеке из-за этого нельзя.
            CoreError::Io(io::Error::new(io::ErrorKind::InvalidData, e))
        })?;
        stmt.execute(params![
            track.id.provider.as_str(),
            track.id.id.as_str(),
            track.title.as_str(),
            artists.as_str(),
            track.album.as_deref(),
            track.duration.map(duration_to_ms),
            track.art_url.as_deref(),
            track.page_url.as_deref(),
            updated_at,
        ])
        .map_err(|e| self.db_error(e))?;
        Ok(())
    }

    fn forget_audio(&self, provider: &str, id: &str) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM audio WHERE provider = ?1 AND id = ?2",
                params![provider, id],
            )
            .map_err(|e| self.db_error(e))?;
        Ok(())
    }

    /// Транзакция по `&self`: `Cache` держат в одном месте и шарят между
    /// задачами, поэтому `&mut self` у методов записи не было бы.
    fn transaction(&self) -> Result<rusqlite::Transaction<'_>> {
        self.conn
            .unchecked_transaction()
            .map_err(|e| self.db_error(e))
    }

    fn db_error(&self, source: rusqlite::Error) -> CoreError {
        CoreError::Database {
            path: self.paths.database(),
            source,
        }
    }
}

/// Кандидат на вытеснение.
struct Victim {
    provider: String,
    id: String,
    path: PathBuf,
    bytes: i64,
}

fn create_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|source| CoreError::Cache {
        path: dir.to_path_buf(),
        source,
    })
}

fn collect<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
    wrap: impl Fn(rusqlite::Error) -> CoreError,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(&wrap)?);
    }
    Ok(out)
}

/// Читает трек из колонок [`TRACK_COLUMNS`].
///
/// Битая строка артистов не превращается в пустой список: молчаливая
/// потеря артистов хуже громкой ошибки о испорченной базе.
fn track_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Track> {
    let provider: String = row.get(0)?;
    let id: String = row.get(1)?;
    let artists_json: String = row.get(3)?;
    let artists: Vec<String> = serde_json::from_str(&artists_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;

    Ok(Track {
        id: TrackId::new(provider_from_db(&provider)?, id),
        title: row.get(2)?,
        artists,
        album: row.get(4)?,
        duration: row.get::<_, Option<i64>>(5)?.and_then(duration_from_ms),
        art_url: row.get(6)?,
        page_url: row.get(7)?,
    })
}

fn playlist_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Playlist> {
    let provider: String = row.get(0)?;
    Ok(Playlist {
        id: PlaylistId::new(provider_from_db(&provider)?, row.get::<_, String>(1)?),
        title: row.get(2)?,
        subtitle: row.get(3)?,
        art_url: row.get(4)?,
        track_count: row
            .get::<_, Option<i64>>(5)?
            .and_then(|count| u32::try_from(count).ok()),
    })
}

/// Имя провайдера из строки базы.
///
/// Разбор один на весь проект — [`ProviderId::from_name`]: второй список
/// констант рядом с первым разошёлся бы с ним молча, и расхождение
/// проявилось бы как «трек есть в базе, но не играется».
///
/// Незнакомое имя — громкая ошибка, а не подстановка «какого-нибудь»
/// провайдера: подстановка смешала бы треки разных сервисов, то есть
/// ровно то, от чего защищает составной ключ.
fn provider_from_db(name: &str) -> rusqlite::Result<ProviderId> {
    ProviderId::from_name(name)
        .ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("провайдер {name} не объявлен в ядре (ProviderId)"),
                )),
            )
        })
}

/// Оставить в имени только `[A-Za-z0-9_-]`, остальное заменить на `_`.
///
/// Точка тоже заменяется — поэтому `..` в имени не собирается, и выйти из
/// каталога кэша через id или расширение невозможно.
fn sanitize(part: &str) -> String {
    part.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

/// Секунды эпохи для `updated_at`/`accessed_at`. Целое, а не строка: по
/// ним сортируется LRU.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}

fn duration_to_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// SQLite хранит только знаковые i64, поэтому счётчики и размеры
/// приходят `i64`. Отрицательное значение здесь — признак испорченной
/// базы; показать 0 честнее, чем гигантское число из переполнения.
fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn duration_from_ms(ms: i64) -> Option<Duration> {
    u64::try_from(ms).ok().map(Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Component;

    fn bench(limit_bytes: u64) -> (tempfile::TempDir, Cache) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = Cache::open(&Paths::under(dir.path()), limit_bytes).expect("open");
        (dir, cache)
    }

    fn track(provider: ProviderId, id: &str, title: &str) -> Track {
        Track {
            id: TrackId::new(provider, id),
            title: title.to_owned(),
            artists: vec!["Артист".to_owned()],
            album: None,
            duration: None,
            art_url: None,
            page_url: None,
        }
    }

    /// Создаёт файл и берёт его на учёт. Размер настоящий: `register_audio`
    /// читает его из файловой системы, подделкой его не обмануть.
    fn put_audio(cache: &Cache, id: &TrackId, bytes: usize) -> PathBuf {
        let path = cache.audio_path(id, "opus");
        std::fs::create_dir_all(path.parent().expect("каталог провайдера")).expect("audio dir");
        std::fs::write(&path, vec![0_u8; bytes]).expect("write");
        cache.register_audio(id, &path, "opus").expect("register");
        path
    }

    /// Правка `accessed_at` напрямую: LRU проверяется на разных временах,
    /// а спать в тестах секундами нельзя.
    fn set_accessed_at(cache: &Cache, id: &TrackId, at: i64) {
        cache
            .conn
            .execute(
                "UPDATE audio SET accessed_at = ?3 WHERE provider = ?1 AND id = ?2",
                params![id.provider.as_str(), id.id.as_str(), at],
            )
            .expect("touch");
    }

    #[test]
    fn track_survives_the_round_trip_with_all_artists() {
        let (_dir, cache) = bench(u64::MAX);
        let stored = Track {
            id: TrackId::new(ProviderId::YTMUSIC, "vid-1"),
            title: "Название".to_owned(),
            artists: vec!["Первый".to_owned(), "Второй".to_owned(), "Третий".to_owned()],
            album: Some("Альбом".to_owned()),
            duration: Some(Duration::from_millis(225_123)),
            art_url: Some("https://art/1".to_owned()),
            page_url: Some("https://page/1".to_owned()),
        };

        cache.put_tracks(std::slice::from_ref(&stored)).expect("put");
        assert_eq!(cache.track(&stored.id).expect("get"), Some(stored));
    }

    #[test]
    fn one_id_under_two_providers_stays_two_records() {
        let (_dir, cache) = bench(u64::MAX);
        let from_yt = track(ProviderId::YTMUSIC, "x", "от YouTube Music");
        let from_sc = track(ProviderId::SOUNDCLOUD, "x", "от SoundCloud");
        cache
            .put_tracks(&[from_yt.clone(), from_sc.clone()])
            .expect("put");

        assert_eq!(cache.track(&from_yt.id).expect("yt"), Some(from_yt));
        assert_eq!(cache.track(&from_sc.id).expect("sc"), Some(from_sc));
    }

    #[test]
    fn playlist_keeps_its_order() {
        let (_dir, cache) = bench(u64::MAX);
        let playlist = PlaylistId::new(ProviderId::YTMUSIC, "LM");
        let tracks: Vec<Track> = (0..5)
            .map(|i| track(ProviderId::YTMUSIC, &format!("t{i}"), &format!("Трек {i}")))
            .collect();

        cache
            .put_playlist_tracks(&playlist, &tracks)
            .expect("put");
        assert_eq!(cache.playlist_tracks(&playlist).expect("get"), tracks);

        // Укороченный плейлист не должен оставить хвост прежних позиций.
        let shorter = tracks[..2].to_vec();
        cache
            .put_playlist_tracks(&playlist, &shorter)
            .expect("put");
        assert_eq!(cache.playlist_tracks(&playlist).expect("get"), shorter);
    }

    #[test]
    fn playlists_can_be_filtered_by_provider() {
        let (_dir, cache) = bench(u64::MAX);
        let from_yt = Playlist {
            id: PlaylistId::new(ProviderId::YTMUSIC, "LM"),
            title: "Лайки".to_owned(),
            subtitle: None,
            art_url: None,
            track_count: Some(42),
        };
        let from_sc = Playlist {
            id: PlaylistId::new(ProviderId::SOUNDCLOUD, "LM"),
            title: "Лайки".to_owned(),
            subtitle: None,
            art_url: None,
            track_count: None,
        };
        cache
            .put_playlists(&[from_yt.clone(), from_sc.clone()])
            .expect("put");

        let mut all = cache.playlists(None).expect("all");
        all.sort_by(|a, b| a.id.cmp(&b.id));
        let mut expected = vec![from_yt.clone(), from_sc.clone()];
        expected.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(all, expected);

        assert_eq!(
            cache.playlists(Some("ytmusic")).expect("filtered"),
            vec![from_yt]
        );
    }

    #[test]
    fn lookup_forgets_an_audio_file_that_disappeared() {
        let (_dir, cache) = bench(u64::MAX);
        let id = TrackId::new(ProviderId::YTMUSIC, "gone");
        let path = put_audio(&cache, &id, 16);
        assert_eq!(cache.lookup_audio(&id).expect("hit"), Some(path.clone()));

        std::fs::remove_file(&path).expect("rm");
        assert_eq!(cache.lookup_audio(&id).expect("miss"), None);

        // Запись обязана уйти вместе с файлом, иначе она навсегда висела
        // бы в учёте и в лимите.
        let stats = cache.stats().expect("stats");
        assert_eq!(stats.tracks, 0);
        assert_eq!(stats.bytes, 0);
    }

    #[test]
    fn lookup_makes_an_audio_file_the_freshest() {
        let (_dir, cache) = bench(6);
        let old = TrackId::new(ProviderId::YTMUSIC, "old");
        let fresh = TrackId::new(ProviderId::YTMUSIC, "fresh");
        let old_path = put_audio(&cache, &old, 6);
        let fresh_path = put_audio(&cache, &fresh, 6);
        set_accessed_at(&cache, &old, 100);
        set_accessed_at(&cache, &fresh, 200);

        // Обращение к старому делает свежим его, а не «fresh»: на этом
        // держится LRU, и без этого вытеснялось бы то, что слушают.
        assert!(cache.lookup_audio(&old).expect("hit").is_some());
        assert_eq!(cache.gc().expect("gc"), 6);

        assert!(old_path.exists());
        assert!(!fresh_path.exists());
    }

    #[test]
    fn gc_evicts_the_oldest_unpinned_and_never_a_pinned() {
        let (_dir, cache) = bench(6);
        let pinned = TrackId::new(ProviderId::YTMUSIC, "pinned");
        let middle = TrackId::new(ProviderId::YTMUSIC, "middle");
        let newest = TrackId::new(ProviderId::YTMUSIC, "newest");
        let pinned_path = put_audio(&cache, &pinned, 6);
        let middle_path = put_audio(&cache, &middle, 6);
        let newest_path = put_audio(&cache, &newest, 6);
        cache
            .set_pinned(std::slice::from_ref(&pinned), true)
            .expect("pin");
        set_accessed_at(&cache, &pinned, 100);
        set_accessed_at(&cache, &middle, 200);
        set_accessed_at(&cache, &newest, 300);

        // Незакреплённых 12 байт при лимите 6: вытесняется ровно один —
        // самый старый из незакреплённых.
        assert_eq!(cache.gc().expect("gc"), 6);
        assert!(!middle_path.exists());
        assert!(newest_path.exists());
        assert!(pinned_path.exists(), "закреплённое не вытесняется никогда");

        // Закреплённые часы в лимит не входят: второй прогон не должен
        // тронуть ни их, ни оставшийся свежий файл.
        assert_eq!(cache.gc().expect("gc"), 0);
        assert!(pinned_path.exists());
        assert!(newest_path.exists());

        let stats = cache.stats().expect("stats");
        assert_eq!(stats.tracks, 2);
        assert_eq!(stats.bytes, 12);
        assert_eq!(stats.limit_bytes, 6);
        assert_eq!(stats.pinned_tracks, 1);
        assert_eq!(stats.pinned_bytes, 6);
    }

    #[test]
    fn audio_path_cannot_escape_the_cache_dir() {
        let (dir, cache) = bench(u64::MAX);
        let root = Paths::under(dir.path()).audio_dir();
        let provider_dir = cache.paths.audio_dir_for("ytmusic");

        for hostile in ["../../etc/passwd", "/etc/passwd", "a/../../b", "..", "трек/../../x"] {
            let id = TrackId::new(ProviderId::YTMUSIC, hostile);
            let path = cache.audio_path(&id, "opus");
            assert!(path.starts_with(&root), "{path:?} ушёл из {root:?}");
            assert!(
                !path.components().any(|c| matches!(c, Component::ParentDir)),
                "{path:?} содержит переход вверх"
            );
            assert_eq!(path.parent(), Some(provider_dir.as_path()), "{path:?}");
        }

        // Расширение приходит от провайдера и режет путь точно так же.
        let sneaky = cache.audio_path(&TrackId::new(ProviderId::YTMUSIC, "ok"), "../../x");
        assert!(sneaky.starts_with(&root), "{sneaky:?} ушёл из {root:?}");
        assert_eq!(sneaky.parent(), Some(provider_dir.as_path()));
    }
}
