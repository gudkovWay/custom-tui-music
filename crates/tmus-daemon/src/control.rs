//! Control-socket демона: newline-delimited JSON на unix-сокете.
//!
//! Здесь нет никакой логики воспроизведения — каждая команда уходит в
//! [`App::handle`], каждое событие приходит из [`App::subscribe`].
//! Причина: MPRIS, трей, Discord и TUI ходят через тот же `App`, и
//! состояние у всех четырёх совпадает по построению, а не по дисциплине.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;

use tmus_core::protocol::{Ack, Cmd, Event, Frame, Payload, Request, Response};

use crate::app::App;

/// Сколько ждать ответа от чужого демона при проверке сокета.
/// Живой демон отвечает (или принимает) мгновенно; таймаут значит «сокет
/// умер вместе с процессом», а не «второй демон задумался».
const LIVENESS_TIMEOUT: Duration = Duration::from_millis(500);

/// Права на файл сокета: по нему управляют воспроизведением и видно
/// содержимое библиотеки аккаунта — читать его может только хозяин.
const SOCKET_MODE: u32 = 0o600;

pub async fn run(app: Arc<App>) -> anyhow::Result<()> {
    let sock_path = app.paths().control_socket().clone();

    // Остаток прошлого запуска — обычное дело после краша: если его не
    // убрать, `bind` упадёт с `Address already in use` и демон не
    // поднимется вовсе. Но сначала убеждаемся, что по сокету никто не
    // отвечает: живой второй демон означает, что стартовать нельзя — два
    // демона на один mpv дали бы два потока событий и непредсказуемое
    // поведение для человека.
    if sock_path.exists() {
        if socket_is_alive(&sock_path).await {
            bail!(
                "control socket {} уже обслуживается другим демоном tmusd",
                sock_path.display()
            );
        }
        tokio::fs::remove_file(&sock_path)
            .await
            .with_context(|| format!("не удалось удалить мёртвый сокет {}", sock_path.display()))?;
    }

    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("не удалось открыть control-сокет {}", sock_path.display()))?;

    // Режим выставляем на уже созданный файл: bind() создаёт сокет с
    // правами процесса (обычно 0755), и между bind и chmod никто
    // подключиться по пути всё равно не может содержательнее, чем
    // соединиться и молчать.
    tokio::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(SOCKET_MODE)).await?;

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                // Ошибка одного клиента не должна ронять сервер и других
                // клиентов — потому каждый в отдельной задаче.
                let app = app.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_conn(app, stream).await {
                        tracing::warn!("control-клиент отключился с ошибкой: {err:#}");
                    }
                });
            }
            Err(err) => {
                // Разовый сбой accept (например, EMFILE) не повод
                // останавливать демон — логируем и продолжаем.
                tracing::error!("accept на control-сокете: {err}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Жив ли сокет: пытаемся подключиться. Кто-то ответил или даже просто
/// принял соединение — демон жив; любая ошибка (включая случай, когда
/// путь вообще не сокет) — остаток, который можно удалять.
async fn socket_is_alive(path: &Path) -> bool {
    tokio::time::timeout(LIVENESS_TIMEOUT, async {
        // Подключение и закрытие: сам факт успешного connect доказывает,
        // что кто-то слушает. Любая ошибка — не сокет или мёртвый путь.
        UnixStream::connect(path).await.map(|_| ())
    })
    .await
    // Таймаут — тоже «мёртвый»: живой демон принимает мгновенно.
    .is_ok_and(|connected| connected.is_ok())
}

async fn handle_conn(app: Arc<App>, stream: UnixStream) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut events = app.subscribe();

    // После `subscribe` соединение становится односторонним: клиент
    // события только читает, входящие строки игнорируются.
    let mut subscribed = false;

    loop {
        tokio::select! {
            line = lines.next_line(), if !subscribed => {
                match line {
                    Ok(Some(line)) => {
                        let request = serde_json::from_str::<Request>(&line);
                        match request {
                            Ok(request) => {
                                if matches!(request.cmd, Cmd::Subscribe) {
                                    // Сначала подтверждение, потом снимок:
                                    // клиент по `ok` знает, что поток пошёл.
                                    send(&mut writer, &Response::Ok {
                                        id: request.id,
                                        ok: Payload::Ack(Ack::default()),
                                    }).await?;
                                    // Первый снимок обязателен: без него
                                    // виджет стоял бы пустым до первой
                                    // смены трека.
                                    let state = app.player().state().await;
                                    send(&mut writer, &Frame::Event(Event::StateChanged { state })).await?;
                                    subscribed = true;
                                } else {
                                    // Решение «гаситься» принимается по
                                    // КОМАНДЕ, а не по ответу: `Shutdown`
                                    // отдаёт такой же `Payload::Ack`, как
                                    // `play`, `pause` и половина остальных
                                    // команд. Проверка по ответу гасила
                                    // демон на первом же `tmus play` —
                                    // замерено на стенде.
                                    let shutdown =
                                        matches!(request.cmd, Cmd::Shutdown);
                                    match app.handle(request.cmd).await {
                                        Ok(payload) => {
                                            send(&mut writer, &Response::Ok {
                                                id: request.id,
                                                ok: payload,
                                            }).await?;
                                            // Ответ уже выдавлен в сокет
                                            // (`write_all` + `flush`), иначе
                                            // `tmus stop` всегда ругался бы
                                            // на обрыв.
                                            if shutdown {
                                                tracing::info!(
                                                    "shutdown по команде control-сокета"
                                                );
                                                // Гасит `main`: он снимает
                                                // файл сокета и убивает mpv.
                                                // `std::process::exit` здесь
                                                // обходил бы обе очистки.
                                                app.request_shutdown();
                                                return Ok(());
                                            }
                                        }
                                        Err(err) => {
                                            send(&mut writer, &Response::Err {
                                                id: request.id,
                                                err: err.to_string(),
                                            }).await?;
                                        }
                                    }
                                }
                            }
                            Err(err) => {
                                // Одна опечатка в скрипте не должна
                                // выкидывать клиента: отвечаем и читаем
                                // дальше. `id: 0` — у мусорного кадра нет
                                // достоверного id, а 0 зарезервирован.
                                send(&mut writer, &Response::Err {
                                    id: 0,
                                    err: format!("неразобранный кадр: {err}"),
                                }).await?;
                            }
                        }
                    }
                    // Клиент закрыл соединение — уходим.
                    Ok(None) => return Ok(()),
                    Err(err) => return Err(err.into()),
                }
            }
            event = events.recv(), if subscribed => {
                match event {
                    Ok(event) => send(&mut writer, &Frame::Event(event)).await?,
                    Err(RecvError::Lagged(skipped)) => {
                        // Отставание — почти всегда потерянные `Position`,
                        // из-за них рвать подписку нельзя. Полный снимок
                        // восстанавливает картину целиком.
                        tracing::warn!("подписчик control-сокета отстал на {skipped} событий");
                        let state = app.player().state().await;
                        send(&mut writer, &Frame::Event(Event::StateChanged { state })).await?;
                    }
                    Err(RecvError::Closed) => {
                        // Шина событий умерла — значит умирает и демон.
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Записать значение одним кадром: строка JSON + `\n`. Перевод строки
/// внутри JSON не появляется никогда — `serde_json::to_string` его не даёт.
async fn send<T: Serialize>(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    value: &T,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmus_core::protocol::Cmd;

    #[test]
    fn flat_frame_parses_to_command() {
        // Регресс: раньше кадр предполагал вложенный {"cmd":{"cmd":"next"}};
        // плоский формат на проводе — контракт для всех клиентов.
        let req: Request = serde_json::from_str(r#"{"id":7,"cmd":"next"}"#).expect("parse");
        assert_eq!(req.id, 7);
        assert!(matches!(req.cmd, Cmd::Next));
    }

    #[test]
    fn frame_with_fields_parses() {
        let req: Request =
            serde_json::from_str(r#"{"id":1,"cmd":"seek_by","delta":-5.0}"#).expect("parse");
        assert_eq!(req.id, 1);
        match req.cmd {
            Cmd::SeekBy { delta } => assert!((delta - (-5.0)).abs() < f64::EPSILON),
            other => panic!("ожидали SeekBy, получили {other:?}"),
        }
    }

    #[test]
    fn error_response_is_one_line() {
        let line = serde_json::to_string(&Response::Err {
            id: 3,
            err: "что-то сломалось".to_string(),
        })
        .expect("serialize");
        assert!(!line.contains('\n'), "внутри кадра не должно быть \\n: {line}");
        assert!(line.starts_with(r#"{"id":3,"err":""#));
    }

    #[tokio::test]
    async fn regular_file_is_not_a_live_socket() {
        // Остаток прошлого запуска может быть и обычным файлом (не только
        // сокетом): оба случая — мёртвый путь, который можно удалять.
        let dir = std::env::temp_dir().join(format!("tmus-control-test-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.expect("mkdir");
        let path = dir.join("tmus.sock");
        tokio::fs::write(&path, b"not a socket").await.expect("write");

        assert!(!socket_is_alive(&path).await);

        tokio::fs::remove_dir_all(&dir).await.expect("cleanup");
    }
}
