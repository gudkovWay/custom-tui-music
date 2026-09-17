//! Клиент control-сокета демона. Знает только протокол из `tmus-core`
//! и не зависит ни от одного провайдера: какими сервисами подключён
//! демон, клиенту знать нельзя — всё приходит как `Track`/`Playlist`.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use tmus_core::protocol::{Cmd, Event, Frame, Payload, Request, Response, SUBSCRIBE_ID};
use tmus_core::Paths;

/// Клиент командного соединения. Подписка на события живёт в другом
/// соединении — после `subscribe` демон переводит сокет в односторонний
/// поток, и команды в нём не работают, поэтому два режима не смешивают.
pub struct Client {
    // BufReader нужен для построчного чтения кадров (read_until);
    // запись идёт через него же — BufReader пробрасывает AsyncWrite.
    stream: BufReader<UnixStream>,
    next_id: u64,
}

impl Client {
    pub async fn connect(paths: &Paths) -> Result<Self> {
        let stream = UnixStream::connect(paths.control_socket())
            .await
            .context("демон не отвечает на control-сокете")?;
        Ok(Self { stream: BufReader::new(stream), next_id: 0 })
    }

    /// Один запрос — один ответ. `id` инкрементальный: протокол требует
    /// ответа с тем же `id`, и при переиспользовании соединения это
    /// защищает от путаницы кадров.
    pub async fn call(&mut self, cmd: Cmd) -> Result<Payload> {
        self.next_id += 1;
        let id = self.next_id;
        let mut frame = serde_json::to_string(&Request { id, cmd })?;
        frame.push('\n');
        self.stream
            .write_all(frame.as_bytes())
            .await
            .context("не удалось отправить команду демону")?;
        self.stream.flush().await?;

        let mut buf = Vec::new();
        let n = self
            .stream
            .read_until(b'\n', &mut buf)
            .await
            .context("обрыв соединения при чтении ответа")?;
        if n == 0 {
            bail!("демон закрыл соединение, не ответив");
        }
        let line = std::str::from_utf8(&buf)?.trim_end();
        match serde_json::from_str::<Frame>(line)? {
            Frame::Response(Response::Ok { ok, .. }) => Ok(ok),
            // Текст ошибки демона отдаём как есть: он уже по-русски и
            // написан для человека, оборачивать его нечему.
            Frame::Response(Response::Err { err, .. }) => Err(anyhow!(err)),
            Frame::Event(_) => Err(anyhow!("демон прислал событие вместо ответа на команду")),
        }
    }

    /// Поток событий. Отдельное соединение: после `subscribe` оно
    /// одностороннее. Задача-читатель умирает вместе с приёмником —
    /// так никто не читает сокет вхолостую после закрытия TUI.
    pub async fn subscribe(paths: &Paths) -> Result<mpsc::Receiver<Event>> {
        let mut stream = Client::connect(paths).await?.stream;
        let mut frame = serde_json::to_string(&Request { id: SUBSCRIBE_ID, cmd: Cmd::Subscribe })?;
        frame.push('\n');
        stream
            .write_all(frame.as_bytes())
            .await
            .context("не удалось отправить подписку демону")?;
        stream.flush().await?;

        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match stream.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(line) = std::str::from_utf8(&buf) else { break };
                if let Ok(Frame::Event(event)) = serde_json::from_str::<Frame>(line.trim_end()) {
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            }
        });
        Ok(rx)
    }

    /// Подключение с автозапуском демона. Сокета может не быть при
    /// первом запуске: предыдущие клиенты требовали ручной подготовки,
    /// здесь демон стартует сам. Запуск отвязанный (`Stdio::null()` на
    /// всё) — демон обязан переживать закрытие TUI, иначе в демоне нет
    /// смысла.
    pub async fn connect_or_spawn(paths: &Paths) -> Result<Self> {
        if let Ok(client) = Self::connect(paths).await {
            return Ok(client);
        }
        spawn_daemon()?;
        // Сокет создаётся демоном при старте; ждём до 10 с поллингом
        // подключения — конкретного сигнала готовости протокол не даёт.
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(client) = Self::connect(paths).await {
                return Ok(client);
            }
        }
        bail!("демон не поднял control-сокет за 10 с: {:?}", paths.control_socket());
    }
}

/// Запустить `tmusd` отвязанно. Сначала ищем рядом со своим бинарем
/// (cargo install и локальный target кладут их в один каталог), иначе —
/// в `PATH`.
fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe().context("не удалось определить свой путь")?;
    let local = exe.parent().unwrap_or(std::path::Path::new(".")).join("tmusd");
    let program = if local.is_file() {
        local
    } else {
        std::path::PathBuf::from("tmusd")
    };
    tokio::process::Command::new(program)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("не удалось запустить tmusd")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Регресс на баг с вложенностью: без `flatten` в `Request` кадр
    /// получался `{"id":1,"cmd":{"cmd":"next"}}`.
    #[test]
    fn request_frame_is_flat() {
        let got = serde_json::to_string(&Request { id: 1, cmd: Cmd::Next }).expect("serialize");
        assert_eq!(got, r#"{"id":1,"cmd":"next"}"#);
    }
}
