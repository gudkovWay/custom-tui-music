//! Подметальщик осиротевших mpv на старте демона.
//!
//! Почему это нужно: инцидент 19.09 — демон погиб без штатного
//! завершения (kill -9, OOM, паника ядра), и его дети-mpv остались
//! жить: к обнаружению накопилось 72 процесса на ~3.4 ГБ RSS. Юнит
//! systemd здесь бессилен: `KillMode=control-group` убивает cgroup
//! только когда юнит останавливается штатно, а при жёсткой смерти
//! демон умирает раньше, чем успевает что-то сделать. Единственный,
//! кто может убрать остатки, — следующее включение демона, поэтому
//! подметаем до поднятия собственного плеера.
//!
//! Критерий «сироты»: в cmdline процесса есть оба маркера
//! `--input-ipc-server=` и `tmus-mpv.sock` (это наши mpv), но cgroup
//! процесса отличается от нашего (свои дети после старта живут в
//! нашем cgroup — их не трогаем; сравнение строкой достаточно:
//! у унаследованных детей путь cgroup совпадает байт в байт).

/// Чистая логика матчинга сироты: cmdline как сырые байты
/// (NUL-разделённые аргументы из /proc/<pid>/cmdline), cgroup
/// кандидата и наш собственный. Выделена отдельно, чтобы
/// тестировать без /proc и без реальных процессов.
fn is_tmus_mpv_orphan(cmdline: &[u8], cgroup: &str, own: &str) -> bool {
    // cmdline NUL-разделён; маркеры могут сидеть в разных аргументах
    // (`--input-ipc-server=` в одном, сокет в его значении), поэтому
    // ищем оба подмаркера по всему массиву байт, а не в одном аргументе.
    let has_ipc = cmdline
        .windows(b"--input-ipc-server=".len())
        .any(|w| w == b"--input-ipc-server=".as_slice());
    let has_sock = cmdline
        .windows(b"tmus-mpv.sock".len())
        .any(|w| w == b"tmus-mpv.sock".as_slice());
    has_ipc && has_sock && cgroup != own
}

/// Перебирает /proc и гасит чужие осиротевшие mpv: SIGTERM, до 2 с
/// на выход, выжившим SIGKILL. Ошибки чтения (процесс умер между
/// листингом и чтением) молча пропускаются — гонка листинга нормальна.
pub fn sweep_orphan_mpv() {
    let own_cgroup = match std::fs::read_to_string("/proc/self/cgroup") {
        Ok(s) => s,
        // Нет /proc (немыслимо на Linux) — подметать нечем и не кого.
        Err(_) => return,
    };
    let own_cgroup = own_cgroup.trim_end();

    let mut orphans = Vec::new();
    let Ok(procfs) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in procfs.flatten() {
        // Каталоги процессов — только цифровые имена.
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let base = format!("/proc/{pid}");
        let (Ok(cmdline), Ok(cgroup)) = (
            std::fs::read(format!("{base}/cmdline")),
            std::fs::read_to_string(format!("{base}/cgroup")),
        ) else {
            // Процесс мог умереть между листингом и чтением — это норма.
            continue;
        };
        if is_tmus_mpv_orphan(&cmdline, cgroup.trim_end(), own_cgroup) {
            orphans.push(pid);
        }
    }
    if orphans.is_empty() {
        return;
    }

    let mut killed = 0u32;
    for pid in &orphans {
        // SIGTERM — вежливая просьба уйти; mpv на неё корректно завершается.
        unsafe {
            libc::kill(*pid as i32, libc::SIGTERM);
        }
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
    for pid in &orphans {
        if std::fs::metadata(format!("/proc/{pid}")).is_ok() {
            // Всё ещё жив после TERM — добиваем.
            unsafe {
                libc::kill(*pid as i32, libc::SIGKILL);
            }
        }
        killed += 1;
    }
    tracing::info!(killed = killed, "подметены осиротевшие mpv");
}


#[cfg(test)]
mod tests {
    use super::is_tmus_mpv_orphan;

    fn cmdline(server: bool, sock: bool) -> Vec<u8> {
        let mut parts: Vec<&str> = vec!["mpv", "--no-video"];
        if server {
            parts.push("--input-ipc-server=/run/user/1000/tmus-mpv.sock");
        }
        if sock {
            parts.push("--extra=tmus-mpv.sock");
        }
        parts.join("\u{0}").into_bytes()
    }

    #[test]
    fn чужой_cgroup_и_оба_маркера_это_сирота() {
        assert!(is_tmus_mpv_orphan(
            &cmdline(true, false),
            "0::/user.slice/user-1000.slice/old.scope",
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/tmusd.service"
        ));
    }

    #[test]
    fn наш_cgroup_не_трогаем() {
        let own = "0::/user.slice/user-1000.slice/tmusd.service";
        assert!(!is_tmus_mpv_orphan(&cmdline(true, false), own, own));
    }

    #[test]
    fn без_маркеров_не_наш_процесс() {
        assert!(!is_tmus_mpv_orphan(
            b"mpv\0--no-video\0",
            "0::/другой/cgroup",
            "0::/наш/cgroup"
        ));
    }

    #[test]
    fn один_маркер_без_второго_недостаточен() {
        let only_ipc: Vec<u8> = b"mpv\0--input-ipc-server=/tmp/other.sock\0".to_vec();
        let only_sock: Vec<u8> = b"mpv\0--sock=tmus-mpv.sock\0".to_vec();
        assert!(!is_tmus_mpv_orphan(&only_ipc, "0::/a", "0::/b"));
        assert!(!is_tmus_mpv_orphan(&only_sock, "0::/a", "0::/b"));
    }
}
