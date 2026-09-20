//! Эксклюзивный запуск демона через `flock`. Один файл — один живой
//! демон; дубликат при старте молча уходит, не поднимая mpv, филлер и
//! персист (инцидент 21.09: шесть демонов одновременно, каждый со своим
//! mpv, и все пишут один `queue.json`).
//!
//! Почему `flock`, а не «файл-флаг»: файл переживает падение процесса,
//! а блокировка — нет. После kill -9/OOM флаг-файл остаётся лежать и
//! навсегда блокирует запуск (или требует стирания руками), flock же
//! снимается ядром вместе с погибшим дескриптором.
//!
//! Почему ждём, а не отступаем сразу: рестарт systemd-юнита гасит
//! старый демон до ~10 с (штатное завершение mpv, MPRIS, трея).
//! Мгновенный отказ на рестарте дал бы флапающий юнит.

use std::io;
use std::os::fd::AsRawFd as _;
use std::path::Path;
use std::time::{Duration, Instant};

/// Держатель эксклюзивной блокировки. Релиз — на закрытии файла при
/// выходе процесса.
pub struct Singleton {
    _file: std::fs::File,
}

/// Взять эксклюзивную блокировку на `path`. Занято — ждём до `wait`
/// шагами по 200 мс; так и не дождались — `Ok(None)`. Любая ошибка,
/// кроме «занято», — `Err`.
pub fn acquire(path: &Path, wait: Duration) -> io::Result<Option<Singleton>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)?;

    let deadline = Instant::now() + wait;
    loop {
        // flock при отказе возвращает -1, а причину кладёт в errno,
        // поэтому читаем last_os_error, а не возвращаемое значение.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(Singleton { _file: file }));
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // EWOULDBLOCK == EAGAIN: замок держит другой процесс.
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            _ => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn second_acquire_with_zero_wait_fails_while_first_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmus.lock");

        let first = acquire(&path, Duration::ZERO).unwrap().expect("первый берёт замок");
        assert!(acquire(&path, Duration::ZERO).unwrap().is_none(), "второй должен получить None");

        drop(first);
    }

    #[test]
    fn lock_is_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmus.lock");

        let first = acquire(&path, Duration::ZERO).unwrap().unwrap();
        drop(first);

        assert!(
            acquire(&path, Duration::ZERO).unwrap().is_some(),
            "после drop замок должен освободиться"
        );
    }

    #[test]
    fn different_paths_do_not_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let a = acquire(&dir.path().join("a.lock"), Duration::ZERO).unwrap().unwrap();
        let b = acquire(&dir.path().join("b.lock"), Duration::ZERO).unwrap().unwrap();
        drop((a, b));
    }
}
