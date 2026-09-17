# tmus

Терминальный музыкальный плеер: демон `tmusd` + клиент `tmus`.
Воспроизведение — через `mpv`, каталог — через провайдеров, интеграция с
оболочкой — через MPRIS и иконку в трее.

Заменяет Electron-приложение. Замерено на одной машине, один и тот же
трек: `ytmdesktop` — **11 процессов, 1329 МБ RSS**; `tmus` — **2 процесса,
52 МБ PSS**.

## Установка

Три шага. Копировать cookie из DevTools не нужно — плеер сам находит
сессию в профиле браузера.

```sh
cargo install --path crates/tmus-tui --path crates/tmus-daemon
install -Dm644 packaging/tmusd.service ~/.config/systemd/user/tmusd.service
systemctl --user enable --now tmusd
```

Нужны `mpv` и `yt-dlp` в системе. Дальше — `tmus`.

## Как это устроено

```
tmus (TUI и подкоманды) ─┐
noctalia / MPRIS ────────┼─→ tmusd ─→ mpv (подпроцесс, JSON-IPC)
Discord RPC ─────────────┘     │
иконка в трее ─────────────────┤─→ провайдеры (каталог + резолв потока)
                               └─→ SQLite + офлайн-кэш аудио
```

Демон отдельно от интерфейса намеренно: музыка играет с закрытым
терминалом, а иконка в трее остаётся. Закрытие TUI ничего не
останавливает.

## Провайдеры

Приложение построено **вокруг абстракции провайдеров, а не вокруг одного
сервиса**. Три трейта: `Account` (авторизация), `Catalog` (поиск,
библиотека, плейлисты), `Resolver` (`TrackId` → откуда играть).

Причина такого разделения: каталог обобщается чисто, а добыча потока —
нет. YouTube Music и SoundCloud резолвятся через `yt-dlp`; Spotify так
резолвиться не может — там свой протокол. Один трейт на оба дела
заставил бы Spotify притворяться yt-dlp-провайдером.

Все фичи — очередь, офлайн-кэш, MPRIS, трей, Discord RPC, TUI — работают
над `Track` и не знают, какой сервис играет. Очередь смешанная: треки
разных провайдеров лежат в ней рядом.

Сейчас реализован YouTube Music.

## Команды

```sh
tmus                       # TUI
tmus toggle | next | prev  # для медиа-клавиш Hyprland
tmus seek +15 | seek 90    # относительно или абсолютно
tmus vol +5  | vol 40
tmus status | library | liked | queue | providers | search <запрос>
tmus play <provider>:<id>  # напр. tmus play ytmusic:dQw4w9WgXcQ
tmus cache stats | pin <id> | unpin <id> | gc
tmus events                # поток событий JSON (им пользуется плагин noctalia)
```

## Конфиг

`~/.config/tmus/config.toml`. Файла нет — работают значения по умолчанию.

```toml
volume = 70.0
mpv = "mpv"
yt_dlp = "yt-dlp"
audio_format = "bestaudio[acodec=opus]/bestaudio"
discord_rpc = true
# Application ID из Discord Developer Portal. Своё приложение
# обязательно: с чужим в профиле было бы чужое имя и чужая иконка.
# Ассеты загружать не нужно — обложка отдаётся внешней ссылкой.
discord_app_id = "0000000000000000000"

# ВНИМАНИЕ: все поля верхнего уровня обязаны идти ДО первой секции в
# квадратных скобках. Ключ ниже `[cache]` TOML относит внутрь `[cache]`.
# Незнакомый ключ демон отвергает с указанием строки, а не проглатывает
# молча — на этом уже попались с `discord_app_id`.

[cache]
limit_bytes = 8589934592   # лимит офлайн-кэша, вытеснение LRU
prefetch_next = true       # докачивать следующий трек заранее

[browser]
# Автодетект обычно справляется; путь нужен, когда профилей несколько.
profile = "chromium:/path/to/profile"

[providers.ytmusic]
enabled = true
```

Discord RPC требует своего приложения: `discord.com/developers/applications`
→ `New Application` → имя (его увидят друзья как «Listening to …») →
скопировать `Application ID` в `discord_app_id`. Ассеты в
`Rich Presence → Art Assets` загружать не нужно: обложка отдаётся
внешним URL. Чужой id подставлять нельзя — в профиле человека
отображалось бы чужое приложение.

Запасной путь — переменная `TMUS_DISCORD_APP_ID`; конфиг важнее её.

## Офлайн

Играемый трек пишется на диск, следующий в очереди докачивается заранее.
Есть файл — провайдер не опрашивается вовсе. Вытеснение LRU по лимиту,
закреплённое (`tmus cache pin`) не вытесняется никогда.

## Интеграция с noctalia

Правок в саму noctalia не требуется: демон публикует
`org.mpris.MediaPlayer2.tmus` и иконку `org.kde.StatusNotifierItem`,
которые она читает штатно.

Виджет в баре, панель и спектр — отдельный плагин
`~/.config/hypr/noctalia-plugins/tmus/`. Спектр берётся у самой noctalia
(её захват PipeWire отдаётся плагинам через `onAudioSpectrum`), поэтому
`cava` не нужен.

## Замеренные грабли

Записаны, потому что каждая выглядит как поломка совсем в другом месте.

- **`player_client` обязательно пинить в `web_music`.** Дефолтный выбор
  `yt-dlp` уходит в `web_creator`: URL резолвится, а GET по нему даёт
  `403 Forbidden`.
- **`mpv --http-header-fields` режет значения по запятым.** Заголовок
  `Accept` от `yt-dlp` содержит запятые → googlevideo отвечает `400`,
  тогда как тот же URL в `curl` даёт `206`. Передаётся только
  `User-Agent`.
- **В `property-change` значение лежит в поле `data`**, а не в поле,
  названном по имени свойства. Чтение из `position`/`duration`/`pause`
  давало `None` на каждом кадре — плеер выглядел молчащим при рабочем
  звуке.
- **URL потока живёт около шести часов** (`expire=`). Кэшируются
  метаданные и файлы, URL — никогда.
- **Cookies Chromium на Linux — AES-128-CBC**, а не GCM: ключ
  `PBKDF2-HMAC-SHA1("peanuts", "saltysalt", 1, 16)`, IV — 16 пробелов,
  при `meta.version >= 24` первые 32 байта открытого значения нужно
  отбросить. Часть cookies (`v11`) зашифрована ключом из keyring и не
  читается — они пропускаются, ошибка только если не набралось ни одной.
- **mpv нужно гасить явно и дожидаться смерти.** `quit` по IPC уходит
  асинхронно, и демон успевает выйти раньше записи — mpv остаётся
  сиротой на ~40 МБ и продолжает играть без управления.

## Лицензия

MIT.

## Медиа-клавиши Hyprland

```conf
bindl = , XF86AudioPlay,  exec, tmus toggle
bindl = , XF86AudioNext,  exec, tmus next
bindl = , XF86AudioPrev,  exec, tmus prev
bindl = , XF86AudioStop,  exec, tmus stop
bind  = SUPER, M,         exec, kitty -e tmus
```

`bindl` вместо `bind` — чтобы клавиши работали и на залоченном экране.
