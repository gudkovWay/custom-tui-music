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
//! Критерий «сироты»: в cmdline процесса есть ровно наш аргумент
//! `--input-ipc-server=<наш сокет>` (это наши mpv), но cgroup процесса
//! отличается от нашего (свои дети после старта живут в нашем cgroup —
//! их не трогаем; сравнение строкой достаточно: у унаследованных детей
//! путь cgroup совпадает байт в байт).
//!
//! Почему сверяется ПОЛНЫЙ путь сокета, а не подстрока `tmus-mpv.sock`:
//! инцидент 21.09.2026 — тестовый демон с изолированными XDG-каталогами
//! (свой `XDG_RUNTIME_DIR`) подметал «чужие» mpv и трижды убил mpv
//! ЖИВОГО демона пользователя: для него чужой cgroup и при этом общий
//! маркер `tmus-mpv.sock` совпадали. Подметать можно только остатки
//! своего рантайма — у изолированного стенда свой сокет, и он теперь
//! неуязвим для живого демона (и наоборот).

/// Чистая логика матчинга сироты: cmdline как сырые байты
/// (NUL-разделённые аргументы из /proc/<pid>/cmdline), cgroup
/// кандидата и наш собственный плюс полный аргумент `--input-ipc-server`
/// с нашим сокетом. Выделена отдельно, чтобы тестировать без /proc и без
/// реальных процессов.
fn is_tmus_mpv_orphan(cmdline: &[u8], cgroup: &str, own: &str, ipc_arg: &str) -> bool {
    // cmdline NUL-разделён; аргумент ищем целиком по всему массиву байт:
    // подстрока одного лишь имени сокета ловила бы чужие рантаймы.
    let ours = cmdline
        .windows(ipc_arg.len())
        .any(|w| w == ipc_arg.as_bytes());
    ours && cgroup != own
}

/// Перебирает /proc и гасит чужие осиротевшие mpv: SIGTERM, до 2 с
/// на выход, выжившим SIGKILL. Ошибки чтения (процесс умер между
/// листингом и чтением) молча пропускаются — гонка листинга нормальна.
pub fn sweep_orphan_mpv(socket: &std::path::Path) {
    let own_cgroup = match std::fs::read_to_string("/proc/self/cgroup") {
        Ok(s) => s,
        // Нет /proc (немыслимо на Linux) — подметать нечем и не кого.
        Err(_) => return,
    };
    let own_cgroup = own_cgroup.trim_end();
    // Ищем именно свой сокет: чужой рантайм (изолированный стенд) —
    // не наш мусор, см. комментарий к `is_tmus_mpv_orphan`.
    let ipc_arg = format!("--input-ipc-server={}", socket.display());

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
        if is_tmus_mpv_orphan(&cmdline, cgroup.trim_end(), own_cgroup, &ipc_arg) {
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

    const OWN_SOCKET: &str = "--input-ipc-server=/run/user/1000/tmus-mpv.sock";
    const OTHER_SOCKET: &str = "--input-ipc-server=/tmp/tmus-stand/run/tmus-mpv.sock";

    fn cmdline(arg: Option<&str>) -> Vec<u8> {
        let mut parts: Vec<&str> = vec!["mpv", "--no-video"];
        if let Some(arg) = arg {
            parts.push(arg);
        }
        parts.join("\u{0}").into_bytes()
    }

    #[test]
    fn чужой_cgroup_со_своим_сокетом_это_сирота() {
        assert!(is_tmus_mpv_orphan(
            &cmdline(Some(OWN_SOCKET)),
            "0::/user.slice/user-1000.slice/old.scope",
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/tmusd.service",
            OWN_SOCKET
        ));
    }

    #[test]
    fn наш_cgroup_не_трогаем() {
        let own = "0::/user.slice/user-1000.slice/tmusd.service";
        assert!(!is_tmus_mpv_orphan(&cmdline(Some(OWN_SOCKET)), own, own, OWN_SOCKET));
    }

    #[test]
    fn без_маркеров_не_наш_процесс() {
        assert!(!is_tmus_mpv_orphan(b"mpv\0--no-video\0", "0::/другой/cgroup", "0::/наш/cgroup", OWN_SOCKET));
    }

    /// Регрессия 21.09.2026: mpv тестового демона с изолированным
    /// `XDG_RUNTIME_DIR` (свой сокет + чужой cgroup) живой демон
    /// подметать НЕ должен — иначе тесты гасят плеер пользователя.
    #[test]
    fn чужой_рантайм_не_подметается() {
        assert!(!is_tmus_mpv_orphan(
            &cmdline(Some(OTHER_SOCKET)),
            "0::/user.slice/user-1000.slice/session.scope",
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/tmusd.service",
            OWN_SOCKET
        ));
        // И симметрично: сам себя стенд со своим сокетом видит, живой — нет.
        assert!(is_tmus_mpv_orphan(
            &cmdline(Some(OTHER_SOCKET)),
            "0::/user.slice/user-1000.slice/session.scope",
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/tmusd.service",
            OTHER_SOCKET
        ));
    }

    /// Подстрока одного лишь имени сокета теперь недостаточна: процесс,
    /// упомянувший `tmus-mpv.sock` без полного аргумента, — не наш mpv.
    #[test]
    fn упоминание_имени_сокета_без_аргумента_не_сирота() {
        assert!(!is_tmus_mpv_orphan(
            &cmdline(Some("--extra=tmus-mpv.sock")),
            "0::/a",
            "0::/b",
            OWN_SOCKET
        ));
    }
}
