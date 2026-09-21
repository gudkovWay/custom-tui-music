--!nonstrict

local KEY = "tmus"
local SERVICE = "q/tmus:watch"

-- Цвета — ИМЕНА РОЛЕЙ палитры хоста, а не hex: хост сам резолвит их в
-- актуальный Material-цвет, поэтому смена темы/обоев перекрашивает
-- панель без правок плагина. Роли проверены живой пробой 19.09;
-- для полупрозрачных заливок роль пишется с альфой "role/alpha".
-- Здесь используются ТОЛЬКО проверенно рендерящиеся: surface,
-- surface_variant, primary, primary/0.20, secondary, secondary/0.28,
-- surface_variant/0.55, on_surface, on_surface_variant, outline,
-- error (primary_container валиден, но НЕ рисуется — невидим).
-- Три различимых
-- заливки строк: фон=surface, курсор=primary/0.20, играет=
-- secondary/0.28; курсор НА играющей — заливка playing плюс рамка
-- border 1px, иначе было бы неразличимо, чей цвет сильнее.
local COLORS = {
  bg = "surface",
  surface = "surface_variant",
  -- Курсорная строка обязана читаться с одного взгляда: проба 18.09
  -- показала, что прежний хозяин не видел её.
  selected = "primary/0.20",
  playing = "secondary/0.28",
  -- Фон нижнего now-playing-блока: раньше численно совпадал с
  -- playing — теперь surface_variant/0.55, приглушённый отличимый
  -- фон.
  npbg = "surface_variant/0.55",
  text = "on_surface",
  muted = "on_surface_variant",
  border = "outline",
  -- До 0.4.0 accent использовался в 9 местах (заголовки, активные
  -- глифы, прогресс), но не был определён — при --!nonstrict туда
  -- тихо уходил nil.
  accent = "primary",
  -- Зарезервировано под будущие ошибки (срыв потока, отказ демона).
  error = "error",
}
local FONT = "Agave Nerd Font"
local RAIL = 64
local GAP = 12
local PAD = 16
local LIST_WINDOW = 10
local CARD_WINDOW = 8
-- Карточек в ряду сетки Library (см. playlistCards): шаг moveY.
local GRID_COLS = 4
-- Окно очереди обязано ВЛЕЗАТЬ в бюджет context-колонки целиком:
-- declarative UI не даёт scrollToIndex, «скролл» — это движущееся окно
-- строк, и контент ниже вьюпорта недостижим с клавиатуры. Замер §17:
-- header + 15 строк ≈749 px при бюджете context ≈669 px (панель
-- 1180×760) — хвост из 2 строк срезался снизу (фидбек 21.09, §19).
-- 13 строк ≈653 px — влезает с запасом.
local QUEUE_WINDOW = 13
local NP_SPEC_BARS = 56
-- Home-лента рекомендаций: видимых полок по Y и карточек в
-- горизонтальном окне полки. Карточек в полке рендерим не больше
-- HOME_CARD_CAP — окно двигается, хвост не держим (счётчик честный:
-- shelf.count хранит число карточек до капа).
local SHELF_WINDOW = 2
local HOME_CARD_WINDOW = 6
local HOME_CARD_CAP = 20

local VIEWS = {
  { id = "home", title = "Home", glyph = "home" },
  { id = "search", title = "Search", glyph = "search" },
  { id = "library", title = "Library", glyph = "library" },
  { id = "clients", title = "Clients", glyph = "server" },
  { id = "eq", title = "Equalizer", glyph = "adjustments-horizontal" },
}
local VIEW_IDS = {}
for i, v in ipairs(VIEWS) do
  VIEW_IDS[v.id] = i
end

local LAYOUT = {
  G = "g", H = "h", J = "j", K = "k", L = "l",
  I = "i", N = "n", O = "o", P = "p", R = "r", S = "s",
  Cyrillic_pe = "g", Cyrillic_er = "h", Cyrillic_o = "j", Cyrillic_el = "k",
  Cyrillic_de = "l", Cyrillic_te = "n", Cyrillic_shcha = "o", Cyrillic_ze = "p",
  Cyrillic_sha = "i", Cyrillic_SHA = "i",
  Cyrillic_PE = "g", Cyrillic_ER = "h", Cyrillic_O = "j", Cyrillic_EL = "k",
  Cyrillic_DE = "l", Cyrillic_TE = "n", Cyrillic_SHCHA = "o", Cyrillic_ZE = "p",
  Cyrillic_KA = "r", Cyrillic_YERU = "s",
  -- f/d — оценка играющего трека; на ЙЦУКЕН это Cyrillic_a/ve (замер
  -- xkbcli), заглавные двойники — по образцу PE/SHA выше.
  F = "f", D = "d",
  Cyrillic_a = "f", Cyrillic_A = "f",
  Cyrillic_ve = "d", Cyrillic_VE = "d",
}

-- Ввод в поиске ведётся ЗДЕСЬ, а не в ui.input: захваченные клавиши
-- (Enter, Space, буквы из capture_keys) до сфокусированного поля не
-- доходят — хост отдаёт их только onKey, и родной submit мёртв
-- (замер 18.09: Enter запускал первый трек списка, а не поиск).
-- Поэтому в режиме ввода onKey сам копит строку, а ui.input — витрина.
-- Латиница приходит одним аккордом в верхнем регистре (сглаживает
-- хост), кириллица — именами кейсимов; таблица снята
-- `xkbcli how-to-type --layout ru`, а не по памяти.
local TEXT_CHARS = {
  A = "a", B = "b", C = "c", D = "d", E = "e", F = "f", G = "g", H = "h",
  I = "i", J = "j", K = "k", L = "l", M = "m", N = "n", O = "o", P = "p",
  Q = "q", R = "r", S = "s", T = "t", U = "u", V = "v", W = "w", X = "x",
  Y = "y", Z = "z",
  ["0"] = "0", ["1"] = "1", ["2"] = "2", ["3"] = "3", ["4"] = "4",
  ["5"] = "5", ["6"] = "6", ["7"] = "7", ["8"] = "8", ["9"] = "9",
  minus = "-", comma = ",", apostrophe = "'", period = ".",
  Cyrillic_a = "а", Cyrillic_be = "б", Cyrillic_ve = "в", Cyrillic_ghe = "г",
  Cyrillic_de = "д", Cyrillic_ie = "е", Cyrillic_io = "ё", Cyrillic_zhe = "ж",
  Cyrillic_ze = "з", Cyrillic_i = "и", Cyrillic_shorti = "й",
  Cyrillic_ka = "к", Cyrillic_el = "л", Cyrillic_em = "м", Cyrillic_en = "н",
  Cyrillic_o = "о", Cyrillic_pe = "п", Cyrillic_er = "р", Cyrillic_es = "с",
  Cyrillic_te = "т", Cyrillic_u = "у", Cyrillic_ef = "ф", Cyrillic_ha = "х",
  Cyrillic_tse = "ц", Cyrillic_che = "ч", Cyrillic_sha = "ш",
  Cyrillic_shcha = "щ", Cyrillic_hardsign = "ъ", Cyrillic_yeru = "ы",
  Cyrillic_softsign = "ь", Cyrillic_e = "э", Cyrillic_yu = "ю",
  Cyrillic_ya = "я", Cyrillic_A = "А", Cyrillic_BE = "Б", Cyrillic_VE = "В",
  Cyrillic_GHE = "Г", Cyrillic_DE = "Д", Cyrillic_IE = "Е", Cyrillic_IO = "Ё",
  Cyrillic_ZHE = "Ж", Cyrillic_ZE = "З", Cyrillic_I = "И",
  Cyrillic_SHORTI = "Й", Cyrillic_KA = "К", Cyrillic_EL = "Л",
  Cyrillic_EM = "М", Cyrillic_EN = "Н", Cyrillic_O = "О", Cyrillic_PE = "П",
  Cyrillic_ER = "Р", Cyrillic_ES = "С", Cyrillic_TE = "Т", Cyrillic_U = "У",
  Cyrillic_EF = "Ф", Cyrillic_HA = "Х", Cyrillic_TSE = "Ц",
  Cyrillic_CHE = "Ч", Cyrillic_SHA = "Ш", Cyrillic_SHCHA = "Щ",
  Cyrillic_HARDSIGN = "Ъ", Cyrillic_YERU = "Ы", Cyrillic_SOFTSIGN = "Ь",
  Cyrillic_E = "Э", Cyrillic_YU = "Ю", Cyrillic_YA = "Я",
}
-- ctrl+BackSpace в поиске: удалить слово слева от курсора, модель
-- readline/VSCode — срезать хвостовые разделители, затем слово, затем
-- снова разделители. ASCII-классы %s/%p не пересекаются с байтами
-- кириллицы (все ≥0x80), поэтому «слово» = буквы+цифры+мультибайт.
-- Автоповтор для ctrl+BackSpace сознательно не подключён: лаг одного
-- кадра съел бы полстроки.
local function killWord(s)
  s = string.gsub(s, "[%s%p]+$", "")
  s = string.gsub(s, "[^%s%p]+$", "")
  return (string.gsub(s, "[%s%p]+$", ""))
end

local inputMode = false

local REPEAT_CYCLE = { none = "track", track = "queue", queue = "none" }
local REPEAT_LABEL = { none = "Repeat off", track = "Repeat track", queue = "Repeat queue" }
-- Компактная подпись у кнопки repeat: глифы различают только
-- none/не-none, режим track и queue смотрятся одинаково — чип снимает
-- неоднозначность. При none не рисуется, чтобы не шуметь.
local REPEAT_SHORT = { none = "off", track = "×1", queue = "∞" }
-- Имена пресетов зашиты зеркально tmus-core::eq_presets(): CLI-флаг
-- `--preset` принимает только точное имя, а синхронный запрос списка
-- панель сделать не может. При расхождении демоны отвергнут имя —
-- чинить вместе с ядром.
local EQ_PRESET_NAMES = {
  "Flat", "Bass Boost", "Vocal Boost", "Pop", "Rock", "Hip-Hop", "Jazz",
  "Classical", "Electronic", "Acoustic", "Deep Bass", "Bright", "Crisp",
  "Live", "Headphones", "Small Speakers", "Laptop", "Car", "Earbuds",
  "Podcast",
}
-- Подписи полос 1:1 с EQ_FREQUENCIES_HZ (32 Гц … 16 кГц).
local EQ_FREQ_LABELS = { "32 Hz", "64 Hz", "125 Hz", "250 Hz", "500 Hz", "1 kHz", "2 kHz", "4 kHz", "8 kHz", "16 kHz" }
local EQ_GAIN_LIMIT = 15
local SEARCH_KINDS = {
  { id = "all", title = "All" },
  { id = "track", title = "Tracks" },
  { id = "artist", title = "Artists" },
  { id = "playlist", title = "Collections" },
}
local GROUP_TITLES = { track = "Tracks", artist = "Artists", playlist = "Collections" }
local SEARCH_KIND_TOKENS = { all = "all", track = "tracks", artist = "artists", playlist = "playlists" }

local snapshot = {}
local view = "home"
local cursor = 1
local windowTop = 1
-- Фокус панели-буфера: 2 — центральный список, 3 — правая колонка
-- очереди. Клавиатуру подключит волна 2, рендер уже подсвечивает
-- заголовок колонки.
local focusBuf = 2
local queueCursor = 1
local queueWindowTop = 1
local followPlaying = true
local selected = nil
local playlistId = ""
local playlistTitle = ""
local playlistArtPath = ""
-- Спектр: CSV-строка значений полос, публикуемая виджетом в лёгкий
-- ключ "tmus.spectrum" (троттлинг ≥125 мс — 8 Гц, тяжёлый snapshot
-- KEY "tmus" не дёргается). nil — бары не рисуются.
local spectrum = nil
-- Ввод имени нового плейлиста (Library): буфер ведёт onKey, как в
-- поиске — захваченные клавиши до сфокусированного поля не доходят.
local plNaming = false
local plName = ""
-- Трек, который надо добавить в только что созданный плейлист (из
-- поповера «в плейлист» → «+ New playlist…»).
local plAddAfterCreate = nil
-- Поповер выбора плейлиста для трека: { track = "prov:id", cursor = n }.
local plPicker = nil
-- Модал подсказок: ? (Shift+7 на ЙЦУКЕН — keysym question в обеих
-- раскладках) раскрывает карту клавиш вместо центрального вида,
-- любая клавиша закрывает. Паттерн plPicker — перекрытие центра без
-- нового API.
local helpShown = false
local query = ""
local lastSearched = nil
-- Home-лента ("tmus.home"): курсор (полка/карточка), верхнее окно
-- полок и левые индексы горизонтальных окон по полке. homeFeed —
-- payload лёгкого ключа; рендер-модель полок кэшируется
-- (homeShelfCache) и перевычисляется ТОЛЬКО в watch-колбэке — draw
-- ходит до 8 Гц, перебор полок на каждом кадре недопустим.
local homeShelf = 1
local homeItem = 1
local homeTop = 1
local homeLeft = {}
local homeFeed = nil
local homeShelfCache = {}
-- Вим-модальность поля поиска: typing=true — insert (буквы идут в
-- запрос), false — normal (j/k листают, Enter активирует). Выход из
-- insert: Esc или пары jj/jk/oo на физических клавишах (о/л на
-- ЙЦУКЕН — это те же j/k); вход: i, / или точка.
local typing = false
local insertPending = nil
local INSERT_ESCAPE_MS = 250
local searchKind = "all"
-- Фильтр выдачи по клиенту-провайдеру (§19): чипы над результатами
-- и `c` в normal — цикл all → ytmusic → soundcloud → … Фильтр чисто
-- локальный: выдача уже лежит в snapshot.results, демон не дёргается.
local searchProvider = "all"
local lastSearchSerial = nil
local doSearch -- defined below; used by chips and submitSearch
local rows -- defined below; used by rateSelected (cursor row lookup)
local activate -- defined below; used by trackRow onClick (клик = Enter)
local maybeLoadMore -- defined below; used by move/g-jumps (догрузка хвоста)

local function poke(event, payload)
  local argv = { "noctalia", "msg", "plugin", SERVICE, "all", event }
  if payload then
    table.insert(argv, payload)
  end
  noctalia.runAsync(argv)
end

-- Следующий/предыдущий пресет относительно текущего по кругу; сам
-- демону не поверяем — применит CLI и вернёт состояние.
local function cyclePreset(step)
  local eq = snapshot.equalizer
  if type(eq) ~= "table" then
    return
  end
  local current = tostring(eq.preset or "")
  local at = 1
  for i, name in ipairs(EQ_PRESET_NAMES) do
    if name == current then
      at = i
      break
    end
  end
  -- preset-on, а не preset: выбор пресета включает эквалайзер —
  -- «листаю пресеты, а он всё равно Off» ломал приёмку 19.09.
  local idx = ((at - 1 + step) % #EQ_PRESET_NAMES) + 1
  poke("eq", "preset-on:" .. EQ_PRESET_NAMES[idx])
end

-- Мини-бар полосы: 15 позиций от центра (8-я), усиление ±15 дБ → до
-- 7 шагов в сторону знака. Символы те же, что в прогрессах панели.
local function eqBar(gain)
  local center = 8
  local cells = {}
  for i = 1, 15 do
    cells[i] = "·"
  end
  local steps = math.floor(math.abs(gain) / EQ_GAIN_LIMIT * 7 + 0.5)
  for i = 1, steps do
    cells[center + (gain >= 0 and i or -i)] = "█"
  end
  return table.concat(cells)
end

-- Ряд полос спектра из CSV-строки ("0.12,0.87,..."). Разбираем каждый
-- draw(): строка крошечная, а кэш усложнил бы синк с троттлингом
-- виджета. Панель НЕ считает темп сама: виджет публикует не чаще 8 Гц
-- (троттлинг ≥125 мс на его стороне — полный render у виджета дорог),
-- панель просто рисует последнее присланное значение. Значения вне
-- [0..1] клампятся: бар высотой value*maxHeight, минимум 1 px, чтобы
-- нулевая полоса оставляла базовую линию.
local function spectrumBars(count, maxHeight, barWidth, gap, fillColor)
  local values = {}
  local at = 1
  for num in string.gmatch(tostring(spectrum or ""), "([^,]+)") do
    if at > count then
      break
    end
    local v = tonumber(num) or 0
    values[at] = math.max(0, math.min(1, v))
    at = at + 1
  end
  local bars = {}
  for i = 1, count do
    local v = values[i] or 0
    bars[i] = ui.box({
      key = "spec-bar" .. i,
      width = barWidth or 6,
      height = math.max(1, math.floor(v * maxHeight + 0.5)),
      radius = 1,
      fill = fillColor or COLORS.accent,
    })
  end
  return ui.row({ key = "spec-bars", gap = gap or 2, align = "center" }, bars)
end

local function clock(seconds)
  local total = math.floor(tonumber(seconds) or 0)
  if total < 0 then
    total = 0
  end
  local h = total // 3600
  local m = (total % 3600) // 60
  local s = total % 60
  if h > 0 then
    return string.format("%d:%02d:%02d", h, m, s)
  end
  return string.format("%d:%02d", m, s)
end

local function seconds(value)
  if type(value) == "table" then
    return (tonumber(value.secs) or 0) + ((tonumber(value.nanos) or 0) / 1000000000)
  end
  return tonumber(value) or 0
end

-- Returns the formatted clock time for an available duration, or nil when the
-- duration is missing/unavailable (should not render as 0:00).
local function durationLabel(value)
  if value == nil then
    return nil
  end
  return clock(seconds(value))
end

local function shorten(text, limit)
  if limit <= 0 or text == "" then
    return ""
  end
  local count = utf8.len(text)
  if count == nil or count <= limit then
    return text
  end
  local cut = utf8.offset(text, limit - 1)
  if cut == nil then
    return text
  end
  return string.sub(text, 1, cut - 1) .. "…"
end

local function idOf(row)
  local id = row.id
  if type(id) ~= "table" then
    return tostring(id or "")
  end
  return tostring(id.provider or "") .. ":" .. tostring(id.id or "")
end

-- Ссылка на плейлист для IPC "pl": СОСТАВНАЯ "provider:id". CLI
-- (`tmus pl add/rm/del`) парсит первый аргумент как TrackId-подобный
-- составной идентификатор и голый id отвергает (живой замер 19.09:
-- «ожидался идентификатор вида <provider>:<id>»). Сервис разбирает
-- payload "^prov:plid:trackComposite$" — первые два сегмента без ':'
-- это плейлист, жадный хвост с двоеточием — трек, коллизий нет.
-- Имя плейлиста после create всё равно чистим от ':'.
local function plRef(p)
  if type(p.id) == "table" then
    local prov = tostring(p.id.provider or "")
    local bare = tostring(p.id.id or "")
    if prov ~= "" and bare ~= "" then
      return prov .. ":" .. bare
    end
    return bare ~= "" and bare or prov
  end
  return tostring(p.id or "")
end

-- Метаданные провайдеров: id -> { glyph, color, authKind, authHint }.
-- Перестраиваем на каждый вызов rows() — таблица маленькая, а snapshot
-- refresh() подменяет целиком, кэш тут не поможет.
local providerMeta = {}

local function rebuildProviderMeta(s)
  providerMeta = {}
  for _, p in ipairs(s.providers or {}) do
    providerMeta[tostring(p.id or "")] = {
      glyph = tostring(p.glyph or ""),
      color = tostring(p.color or ""),
      authKind = tostring(p.authKind or ""),
      authHint = tostring(p.authHint or ""),
    }
  end
end

-- Глиф-бейдж провайдера: "☁ " / "♪ ", пусто — если провайдер неизвестен.
local function providerGlyph(providerId)
  local m = providerMeta[tostring(providerId or "")]
  if m == nil or m.glyph == "" then
    return ""
  end
  return m.glyph .. " "
end

-- id провайдера трека: у очереди item.id — таблица {provider, id},
-- у строк поиска/библиотеки — составная строка "provider:id".
local function providerByTrack(t)
  local id = type(t) == "table" and t.id or nil
  if type(id) == "table" then
    return tostring(id.provider or "")
  end
  return tostring(id or ""):match("^[^:]*") or ""
end

-- Рендер-модель home-ленты: массив полок {title, subtitle, count,
-- cards} из homeFeed. count — ЧЕСТНОЕ число элементов полки до капа
-- (счётчик окна показывает его), cards капятся HOME_CARD_CAP —
-- рендерим окно, хвост не держим. Пустые полки выбрасываются.
-- Перевычисляется только в watch-колбэке (rebuildHomeShelves) — draw
-- до 8 Гц не должен перебирать полки.
local function rebuildHomeShelves()
  local out = {}
  if type(homeFeed) == "table" and homeFeed.status == "ok" then
    for _, shelf in ipairs(homeFeed.shelves or {}) do
      local items = shelf.items or {}
      local cards = {}
      for _, it in ipairs(items) do
        if #cards >= HOME_CARD_CAP then
          break
        end
        local kind = tostring(it.kind or "")
        if kind == "track" then
          table.insert(cards, {
            kind = "track",
            id = idOf(it),
            title = tostring(it.title or ""),
            subtitle = type(it.artists) == "table" and table.concat(it.artists, ", ") or "",
            art = tostring(it.art_path or ""),
            time = durationLabel(it.duration),
            event = "play",
            payload = idOf(it),
          })
        elseif kind == "playlist" then
          table.insert(cards, {
            kind = "playlist",
            id = idOf(it),
            title = tostring(it.title or ""),
            subtitle = tostring(it.subtitle or ""),
            art = tostring(it.art_path or ""),
            open = idOf(it),
            openTitle = tostring(it.title or ""),
            openArt = tostring(it.art_path or ""),
          })
        end
      end
      if #cards > 0 then
        table.insert(out, {
          title = tostring(shelf.title or ""),
          subtitle = tostring(shelf.subtitle or ""),
          count = #items,
          cards = cards,
        })
      end
    end
  end
  homeShelfCache = out
end

-- Кэш рендер-модели home-ленты; наполняется только watch-колбэком.
local function homeShelves()
  return homeShelfCache
end

-- Оценка трека из snapshot.ratings (map "provider:id" →
-- "liked"/"disliked"); nil — оценки нет.
local function ratingOf(s, idstr)
  local r = type(s) == "table" and s.ratings or nil
  if type(r) ~= "table" then
    return nil
  end
  return r[idstr]
end

-- Дизлайкнутые по умолчанию спрятаны; ctrl+d показывает их снова.
local showHidden = false

local function hiddenByRating(s, idstr)
  if showHidden then
    return false
  end
  return ratingOf(s, idstr) == "disliked"
end

-- Оценка играющего трека (решение хозяина: цель f/d/a — играющий, не
-- курсор). Фолбэк на строку курсора — только когда ничего не играет.
-- Повторное f/d на той же оценке снимает её: шлём "none". Панель
-- сверяется со своим snapshot: актуальность гарантирует событие
-- rating_changed, обновляющее snapshot.ratings раньше клавиши.
local function rateSelected(want)
  local idstr = tostring(snapshot.trackId or "")
  if idstr == "" then
    if view == "home" then
      local shelf = homeShelves()[homeShelf]
      local c = shelf and shelf.cards[homeItem]
      if c and c.kind == "track" then
        idstr = c.id
      end
    else
      local row = rows()[cursor]
      if row and row.kind == "track" and row.id then
        idstr = row.id
      end
    end
  end
  if idstr == "" then
    return
  end
  -- В протоколе демон ждёт like|dislike|none: "liked"/"disliked" — это
  -- сериализация рейтинга в снапшоте, не payload (сервис фильтр
  -- расширен, но шлём каноническое).
  local word = (want == "liked") and "like" or "dislike"
  local send = (ratingOf(snapshot, idstr) == want) and "none" or word
  poke("rate", send .. ":" .. idstr)
end

-- Оценка в буфере очереди (буфер 3): та же семантика, что у
-- rateSelected — играющий трек первым, фолбэк строка курсора очереди.
-- Повторное f/d на той же оценке снимает её — шлём "none".
local function rateQueueRow(want)
  local idstr = tostring(snapshot.trackId or "")
  if idstr == "" then
    local t = (snapshot.queue or {})[queueCursor]
    idstr = t and idOf(t) or ""
  end
  if idstr == "" then
    return
  end
  local word = (want == "liked") and "like" or "dislike"
  local send = (ratingOf(snapshot, idstr) == want) and "none" or word
  poke("rate", send .. ":" .. idstr)
end

local function trackTitle(t)
  local artists = type(t.artists) == "table" and table.concat(t.artists, ", ") or ""
  return artists ~= "" and (artists .. " — " .. tostring(t.title or "")) or tostring(t.title or "")
end

local function lbl(text, props)
  local p = props or {}
  return ui.label({
    key = p.key,
    text = text,
    color = p.color or COLORS.text,
    fontSize = p.size,
    fontFamily = FONT,
    flexGrow = p.grow,
    width = p.width,
    maxWidth = p.maxWidth,
    opacity = p.opacity,
  })
end

local function card(children, props)
  local p = props or {}
  return ui.column({
    key = p.key,
    gap = p.gap or 10,
    padding = p.padding or PAD,
    radius = p.radius or 10,
    fill = p.fill or COLORS.surface,
    border = p.border,
    borderWidth = p.borderWidth,
    flexGrow = p.grow,
    width = p.width,
    maxWidth = p.maxWidth,
  }, children)
end

local function artNode(path, size, glyph)
  local art = tostring(path or "")
  if art ~= "" then
    return ui.image({
      key = "art",
      path = art,
      width = size,
      height = size,
      radius = 6,
      fit = "cover",
    })
  end
  return ui.column({
    key = "art-fallback",
    width = size,
    height = size,
    radius = 6,
    fill = COLORS.selected,
    align = "center",
  }, {
    ui.glyph({ key = "glyph", name = glyph or "music", color = COLORS.muted }),
  })
end

-- Уровень громкости (0..100) → имя глифа: 0 — volume-zero, до 30 —
-- volume-low, до 70 — volume-2, выше — volume-high.
local function volumeGlyph(v)
  if v <= 0 then
    return "volume-zero"
  elseif v < 30 then
    return "volume-low"
  elseif v < 70 then
    return "volume-2"
  end
  return "volume-high"
end

-- Черновик громкости на время перетаскивания слайдера: onChange
-- держит его локально (демона не дёргаем), onDragEnd вызывается
-- БЕЗ аргумента (std::function<void()>), поэтому берём черновик —
-- и один раз шлём poke("vol-set", абсолютное значение строкой).
-- nil — значение берётся из s.volume.
local volDraft = nil

-- Компактный контрол громкости: глиф-тир + слайдер 0..100 шагом 5.
-- tooltip в host/API 24 нет ни у глифов/слайдеров, ни у контейнеров
-- (проба: prop ignored) — значение и бинды читаются с самого
-- контрола и модала подсказок (?).
local function volumeNode(s)
  local current = volDraft or math.max(0, math.min(100, math.floor(tonumber(s.volume) or 0)))
  return ui.row({
    key = "np-volume",
    gap = 6,
    align = "center",
  }, {
    ui.glyph({
      key = "np-volume-glyph",
      name = volumeGlyph(current),
      size = 14,
      color = COLORS.muted,
    }),
    ui.slider({
      key = "np-volume-slider",
      min = 0,
      max = 100,
      step = 5,
      value = current,
      width = 96,
      onChange = function(v)
        volDraft = math.max(0, math.min(100, math.floor(tonumber(v) or 0)))
      end,
      onDragEnd = function()
        local final = volDraft or current
        volDraft = nil
        poke("vol-set", tostring(math.max(0, math.min(100, math.floor(tonumber(final) or 0)))))
      end,
    }),
  })
end

-- Нижний now-playing: одна карточка на каждый вид, последний ребёнок
-- workspace-колонки. Кнопки/прогресс/спектр живут только здесь —
-- хиро home-вида остаётся чистой типографикой.
local function npNode(s)
  local hasTrack = s.linked and (s.title or "") ~= ""
  local dur = tonumber(s.duration) or 0
  local pos = tonumber(s.position) or 0
  local children = {}
  if hasTrack then
    local marks = {}
    if s.offline then
      table.insert(marks, "Cached")
    end
    if s.shuffle then
      table.insert(marks, "Shuffled")
    end
    if (s.loop or "none") ~= "none" then
      table.insert(marks, REPEAT_LABEL[tostring(s.loop)] or tostring(s.loop))
    end
    local controls = {
      ui.button({
        key = "prev",
        glyph = "player-skip-back",
        controlSize = "sm",
        variant = "ghost",
        onClick = function()
          poke("prev")
        end,
      }),
      ui.button({
        key = "toggle",
        glyph = s.status == "playing" and "player-pause" or "player-play",
        controlSize = "sm",
        variant = "primary",
        onClick = function()
          poke("toggle")
        end,
      }),
      ui.button({
        key = "next",
        glyph = "player-skip-forward",
        controlSize = "sm",
        variant = "ghost",
        onClick = function()
          poke("next")
        end,
      }),
      ui.button({
        key = "shuffle",
        glyph = "arrows-shuffle",
        controlSize = "sm",
        variant = s.shuffle and "primary" or "ghost",
        onClick = function()
          poke("shuffle", "toggle")
        end,
      }),
      ui.button({
        key = "repeat",
        -- Три различимых состояния: track — «repeat-once», queue —
        -- «repeat», оба primary; none — тусклый ghost.
        glyph = (tostring(s.loop or "none")) == "track" and "repeat-once" or "repeat",
        controlSize = "sm",
        variant = (tostring(s.loop or "none")) ~= "none" and "primary" or "ghost",
        onClick = function()
          poke("repeat", REPEAT_CYCLE[tostring(s.loop or "none")] or "none")
        end,
      }),
    }
    local loopMode = tostring(s.loop or "none")
    if loopMode ~= "none" then
      table.insert(controls, ui.label(REPEAT_SHORT[loopMode] or loopMode, {
        key = "np-repeat-short",
        size = 11,
        color = COLORS.muted,
      }))
    end
    table.insert(controls, volumeNode(s))
    table.insert(children, ui.row({ key = "np-top", gap = 12, align = "center" }, {
      artNode(s.artPath, 48),
      ui.column({ key = "np-text", gap = 2, flexGrow = 1 }, {
        lbl(providerGlyph(s.provider) .. shorten(tostring(s.title or ""), 48), { key = "np-title", size = 15, color = COLORS.text }),
        lbl(shorten(tostring(s.artist or ""), 48), { key = "np-artist", size = 12, color = COLORS.muted }),
        -- Причина отказа видна и без играющего трека — иначе она
        -- нужна именно тогда, когда трека нет.
        (tostring(s.lastError or "") ~= "") and lbl(shorten("! " .. tostring(s.lastError), 56), { key = "np-error", size = 11, color = COLORS.error }) or nil,
        lbl(#marks > 0 and table.concat(marks, " · ") or "", {
          key = "np-marks",
          size = 11,
          color = COLORS.muted,
        }),
      }),
      lbl(clock(pos) .. (dur > 0 and (" / " .. clock(dur)) or ""), {
        key = "np-time",
        size = 12,
        color = COLORS.muted,
      }),
      ui.row({ key = "np-controls", gap = 6, align = "center" }, controls),
    }))
    if dur > 0 then
      table.insert(children, ui.progress({
        key = "np-progress",
        progress = math.max(0, math.min(1, pos / dur)),
        flexGrow = 1,
        height = 4,
        radius = 2,
      }))
    end
  else
    -- Без трека геометрия та же: карточка с текстом вместо кнопок —
    -- соседние вью не прыгают при старте/остановке демона.
    table.insert(children, ui.row({ key = "np-top", gap = 12, align = "center" }, {
      ui.glyph({
        key = "np-state-glyph",
        name = s.linked and "music" or "plug-off",
        color = COLORS.muted,
      }),
      ui.column({ key = "np-text", gap = 2, flexGrow = 1 }, {
        lbl(s.linked and "Nothing playing" or "tmusd is not responding", {
          key = "np-state-title",
          size = 15,
          color = COLORS.text,
        }),
        s.linked and nil or lbl("start it with: systemctl --user start tmusd", {
          key = "np-state-hint",
          size = 12,
          color = COLORS.muted,
        }),
        -- Ошибка воспроизведения чаще всего остаётся при пустом треке —
        -- без этой строки панель молчит там, где она нужнее всего.
        (tostring(s.lastError or "") ~= "") and lbl(shorten("! " .. tostring(s.lastError), 56), { key = "np-error", size = 11, color = COLORS.error }) or nil,
      }),
    }))
  end
  -- Полоса спектра ВСЕГДА присутствует: без живого спектра или на
  -- паузе — «спящая» базовая линия 1 px цветом border, чтобы высота
  -- карточки не прыгала на каждом кадре.
  local specColor = (spectrum and s.status == "playing") and COLORS.accent or COLORS.border
  table.insert(children, ui.row({ key = "np-spec", height = 18, align = "center" }, {
    spectrumBars(NP_SPEC_BARS, 14, 5, 2, specColor),
  }))
  return card(children, { key = "np-block", fill = COLORS.npbg, gap = 8, padding = 12 })
end

local function trackRow(i, t, opts)
  local o = opts or {}
  local active = i == cursor
  local current = o.current
  -- Маркер рейтинга перед названием: ♥ лайк, × дизлайк; нет — ничего.
  local marker = o.rating == "liked" and "♥ " or (o.rating == "disliked" and "× " or "")
  local children = {
    ui.row({ key = "mark" .. i, width = 28, align = "center" }, {
      lbl(current and "▶" or (o.index or tostring(i)), {
        key = "idx" .. i,
        color = current and COLORS.accent or COLORS.muted,
      }),
    }),
    artNode(o.art, 32),
    -- Бейдж провайдера перед названием (и рейтинг-маркером).
    lbl(shorten(providerGlyph(providerByTrack(t)) .. marker .. o.title, 52), {
      key = "title" .. i,
      grow = 1,
      color = current and COLORS.accent or (active and COLORS.text or COLORS.muted),
    }),
    lbl(shorten(o.subtitle, 32), { key = "sub" .. i, color = COLORS.muted, maxWidth = 220 }),
    lbl(o.time or "", { key = "time" .. i, color = COLORS.muted, width = 56 }),
  }
  return ui.row({
    key = "track" .. i,
    gap = 10,
    paddingH = 10,
    paddingV = 6,
    radius = 6,
    align = "center",
    -- Играющий трек важнее курсора: у них разные заливки, иначе
    -- «что сейчас играет» не отличить от «где курсор» при скролле.
    fill = current and COLORS.playing or (active and COLORS.selected or COLORS.bg),
    onClick = function()
      cursor = i
      selected = o
      if o.event then
        -- Клик по треку поиска — тот же контекстный запуск, что и
        -- Enter (activate), иначе клик и клавиша играют по-разному.
        if view == "search" and o.kind == "track" then
          activate(o)
        else
          poke(o.event, o.payload)
        end
      end
      draw()
    end,
  }, children)
end

local function resultRow(r)
  local kind = tostring(r.kind or "")
  local artists = type(r.artists) == "table" and table.concat(r.artists, ", ") or ""
  local row = {
    kind = kind,
    id = idOf(r),
    title = tostring(r.title or r.name or ""),
    subtitle = artists ~= "" and artists or tostring(r.album or ""),
    art = tostring(r.art_path or ""),
  }
  if kind == "track" then
    row.rating = ratingOf(snapshot, row.id)
    row.event = "play"
    row.payload = idOf(r)
  elseif kind == "playlist" then
    row.event = "playlist"
    row.payload = idOf(r)
  end
  return row
end

local function searchRows(s)
  local want = searchKind
  local out = {}
  for _, r in ipairs(s.results or {}) do
    local kind = tostring(r.kind or "")
    if kind == "" then
      if type(r.artists) == "table" or type(r.album) == "string" then
        kind = "track"
      end
    end
    -- Фильтр по клиенту (§19): рядом с фильтром вида, чисто локально.
    local provOk = searchProvider == "all" or providerByTrack(r) == searchProvider
    if provOk and (want == "all" or want == kind) and GROUP_TITLES[kind] then
      local row = resultRow(r)
      row.kind = kind
      -- Фильтр дизлайков касается только треков-результатов.
      local hidden = row.kind == "track" and hiddenByRating(s, row.id or "")
      if not hidden then
        table.insert(out, row)
      end
    end
  end
  return out
end

-- Мемо строк вью: rows() зовётся из draw() на каждом кадре (до 8 Гц),
-- а с ленивой догрузкой перебирает весь плейлист без капа. Кэш живёт,
-- пока не сменились вид, плейлист, показ скрытых, серийный номер
-- поиска И сам snapshot — ссылка на таблицу (refresh() подменяет её
-- целиком; точечные патчи "tmus.now" и спектр её не трогают).
local rowsCache = nil
local rowsCacheKey = nil
local rowsCacheSnap = nil

function rows()
  local s = snapshot
  rebuildProviderMeta(s)
  local key = view .. "|" .. playlistId .. "|" .. tostring(showHidden) .. "|" .. tostring(s.searchSerial or "") .. "|" .. searchProvider
  if rowsCache ~= nil and rowsCacheKey == key and rowsCacheSnap == s then
    return rowsCache
  end
  local out = {}

  if view == "library" then
    for _, p in ipairs(s.playlists or {}) do
      local count = tonumber(p.track_count)
      table.insert(out, {
        kind = "playlist",
        title = tostring(p.title or ""),
        subtitle = count and (tostring(count) .. " tracks") or tostring(p.subtitle or ""),
        art = tostring(p.art_path or ""),
        provider = type(p.id) == "table" and tostring(p.id.provider or "") or "",
        -- Голая ссылка для del (см. plRef): двоеточий нет.
        plid = plRef(p),
        open = idOf(p),
        openTitle = tostring(p.title or ""),
        openArt = tostring(p.art_path or ""),
      })
    end
  elseif view == "playlist" then
    if tostring(s.tracksId or "") == playlistId then
      for i, t in ipairs(s.tracks or {}) do
        local idstr = idOf(t)
        if not hiddenByRating(s, idstr) then
          table.insert(out, {
            kind = "track",
            id = idstr,
            rating = ratingOf(s, idstr),
            title = trackTitle(t),
            subtitle = tostring(t.album or ""),
            art = tostring(t.art_path or ""),
            event = "playlist-at",
            payload = tostring(i - 1) .. "|" .. playlistId,
            time = durationLabel(t.duration),
          })
        end
      end
      -- Приглашение хвоста: пока сервис не сказал «дочитано», под
      -- списком живёт приглушённая строка — не активная, без event.
      if s.hasMore ~= false then
        table.insert(out, { kind = "more", title = "… догрузка" })
      end
    end
  elseif view == "search" then
    out = searchRows(s)
  elseif view == "clients" then
    table.insert(out, {
      kind = "client",
      title = "All",
      subtitle = "every connected client",
      event = "source",
      payload = "all",
      active = tostring(s.source or "all") == "all",
    })
    for _, p in ipairs(s.providers or {}) do
      table.insert(out, {
        kind = "client",
        title = p.name,
        subtitle = p.id,
        event = "source",
        payload = p.id,
        active = tostring(s.source or "all") == p.id,
      })
    end
    -- Действие, а не провайдер: полный перезапуск демона. Оно же —
    -- способ перечитать cookies браузера без терминала: залогинился в
    -- SoundCloud — жми сюда. Сокет обрывается, watch сервиса
    -- переподключается и перечитывает состояние.
    table.insert(out, {
      kind = "client",
      title = "Restart tmusd",
      subtitle = "перезапустить демон: перечитать cookies и библиотеки",
      event = "restart-daemon",
      payload = "1",
      restart = true,
    })
  elseif view == "eq" then
    -- Старый демон (без equalizer в PlayerState) не даёт строк —
    -- вью покажет заглушку «eq недоступен».
    local eq = s.equalizer
    if type(eq) == "table" then
      table.insert(out, {
        kind = "eq-power",
        title = eq.enabled == true and "On" or "Off",
        event = "eq",
        payload = eq.enabled == true and "off" or "on",
      })
      table.insert(out, { kind = "eq-preset", title = tostring(eq.preset or "") })
      local bands = type(eq.bands) == "table" and eq.bands or {}
      for i, g in ipairs(bands) do
        table.insert(out, {
          kind = "eq-band",
          index = i,
          gain = tonumber(g) or 0,
          freq = EQ_FREQ_LABELS[i] or tostring(i),
        })
      end
    end
  end

  rowsCacheKey = key
  rowsCacheSnap = s
  rowsCache = out
  return out
end

local function windowSize()
  return view == "library" and CARD_WINDOW or LIST_WINDOW
end

-- Общая арифметика окна списка: держит курсор в границах, окно —
-- за курсором. Возвращает пару (cursor, windowTop), чтобы одну
-- функцию использовали и центральный список, и правая очередь.
local function clampScroll(cur, top, count, window)
  if count == 0 then
    return 1, 1
  end
  if cur < 1 then
    cur = 1
  elseif cur > count then
    cur = count
  end
  local maxTop = math.max(1, count - window + 1)
  if top < 1 then
    top = 1
  elseif top > maxTop then
    top = maxTop
  end
  if cur < top then
    top = cur
  elseif cur >= top + window then
    top = cur - window + 1
  end
  if top > maxTop then
    top = maxTop
  end
  return cur, top
end

local function clampCursor(items)
  cursor, windowTop = clampScroll(cursor, windowTop, #items, windowSize())
end

local function clampQueue(queue)
  queueCursor, queueWindowTop = clampScroll(queueCursor, queueWindowTop, #queue, QUEUE_WINDOW)
end

local function visibleWindow(items)
  local out = {}
  local last = math.min(windowTop + windowSize() - 1, #items)
  for i = windowTop, last do
    table.insert(out, items[i])
  end
  return out, windowTop
end

local function openPlaylist(row)
  playlistTitle = row.openTitle or row.title
  playlistId = row.open
  playlistArtPath = row.openArt or ""
  view = "playlist"
  cursor = 1
  windowTop = 1
  poke("tracks", row.open)
end

-- Навигация home-ленты: курсор (полка, карточка) и окна. Все ходы
-- клампятся без wrap; пустых полок в модели нет (выбрасываются при
-- построении), поэтому «пропуск пустых» — это кламп к границам.
local function clampHomeCursor()
  local shelves = homeShelves()
  if #shelves == 0 then
    homeShelf = 1
    homeItem = 1
    homeTop = 1
    return
  end
  homeShelf, homeTop = clampScroll(homeShelf, homeTop, #shelves, SHELF_WINDOW)
  local shelf = shelves[homeShelf]
  local left = homeLeft[homeShelf] or 1
  homeItem, left = clampScroll(homeItem, left, #shelf.cards, HOME_CARD_WINDOW)
  homeLeft[homeShelf] = left
end

local function homeMoveY(dy)
  local shelves = homeShelves()
  if #shelves == 0 then
    return false
  end
  local target = math.max(1, math.min(#shelves, homeShelf + dy))
  if target == homeShelf then
    return false
  end
  homeShelf = target
  clampHomeCursor()
  return true
end

local function homeMoveX(dx)
  local shelves = homeShelves()
  if #shelves == 0 then
    return false
  end
  local shelf = shelves[homeShelf]
  local target = math.max(1, math.min(#shelf.cards, homeItem + dx))
  if target == homeItem then
    return false
  end
  homeItem = target
  clampHomeCursor()
  return true
end

-- Контекстный запуск (решение хозяина 20.09): активация трека играет
-- весь видимый список как новый контекст, а не одиночный трек в старую
-- очередь. Сбор — только в момент активации: rows() строится на каждом
-- кадре, а трогать его нельзя. items — уже отфильтрованный порядок
-- отображения (rows() для поиска, shelf.cards для home). Возвращает
-- payload "<start>|<id1>,<id2>,..." или nil, если активный трек в
-- списке не найден (снапшот успел смениться) — вызывающий падает
-- назад на одиночный play.
local function contextPayload(activeId, items)
  local ids, start = {}, nil
  for _, it in ipairs(items) do
    if it.kind == "track" and it.id ~= nil and it.id ~= "" then
      if it.id == activeId then
        start = #ids
      end
      table.insert(ids, it.id)
    end
  end
  if start == nil or #ids == 0 then
    return nil
  end
  return tostring(start) .. "|" .. table.concat(ids, ",")
end

local function homeActivate()
  local shelf = homeShelves()[homeShelf]
  local c = shelf and shelf.cards[homeItem]
  if c == nil then
    return
  end
  if c.kind == "playlist" then
    openPlaylist(c)
  elseif c.kind == "track" then
    -- Контекст всей текущей полки: трек-карточки в порядке отображения,
    -- старт с выбранной карточки (зеркало поиска).
    local payload = contextPayload(c.id, shelf.cards)
    if payload then
      poke("play-context", payload)
    else
      poke(c.event or "play", c.payload)
    end
  else
    poke(c.event or "play", c.payload)
  end
end

function activate(row)
  if row == nil then
    return
  end
  selected = row
  if row.kind == "eq-preset" then
    -- Имя берём из живого snapshot: между рендером строки и нажатием
    -- пресет мог смениться (h/l листает соседний). preset-on, а не
    -- preset: Enter на пресете не просто «применяет» — он включает
    -- эквалайзер, иначе «выбрал пресет, а он всё ещё Off».
    local eq = snapshot.equalizer
    if type(eq) == "table" then
      poke("eq", "preset-on:" .. tostring(eq.preset or ""))
    end
    return
  end
  if row.open then
    openPlaylist(row)
  elseif row.event == "play" then
    -- Трек-строка поиска (event "play" ставит только resultRow):
    -- играем контекст всей видимой выдачи в порядке rows(), стартуя
    -- с активированной строки. Плейлистные ("playlist") и вью
    -- playlist ("playlist-at") строки не трогаем.
    local payload = contextPayload(row.id, rows())
    if payload then
      poke("play-context", payload)
    else
      poke(row.event, row.payload)
    end
  elseif row.event then
    poke(row.event, row.payload)
  end
end

local function move(delta)
  local items = rows()
  if #items == 0 then
    return
  end
  cursor = cursor + delta
  if cursor < 1 then
    cursor = #items
  elseif cursor > #items then
    cursor = 1
  end
  clampCursor(items)
  selected = items[cursor]
  maybeLoadMore()
end

-- Курсор очереди (буфер 3): кламп 1..#snapshot.queue без wrap —
-- очередь до 1000 треков, прыжок с конца в начало на зажатой j
-- дезориентирует. Ручной ход выключает автоскролл за играющим.
local function moveQueue(delta)
  local queue = snapshot.queue or {}
  if #queue == 0 then
    return
  end
  followPlaying = false
  queueCursor = math.max(1, math.min(#queue, queueCursor + delta))
  clampQueue(queue)
end

-- Ленивая догрузка хвоста плейлиста: курсор подошёл ближе 20 строк к
-- концу — просим сервис следующую страницу. Шторм гасят три слоя:
-- in-flight у сервиса, hasMore=false после пустой страницы и локальный
-- дебаунс — повторные poke ближе 500 мс (и повторители удержания j)
-- схлопываются без таймеров.
local MORE_MARGIN = 20
local MORE_DEBOUNCE_MS = 500
local lastMoreAt = 0
maybeLoadMore = function()
  if view ~= "playlist" then
    return
  end
  local s = snapshot
  if tostring(s.tracksId or "") ~= playlistId or s.hasMore == false then
    return
  end
  if cursor < #rows() - MORE_MARGIN then
    return
  end
  local now = noctalia.nowMs()
  if now - lastMoreAt < MORE_DEBOUNCE_MS then
    return
  end
  lastMoreAt = now
  poke("tracks-more", playlistId)
end

-- Горизонтальный/вертикальный ход по сетке Library (буфер 2):
-- карточки лежат рядами по GRID_COLS, поэтому строка = ±GRID_COLS.
-- Без wrap: l на краю не прыгает, как в vim. Возвращают needsDraw —
-- так же, как move/handleViewKeys, их используют повторители
-- onFrameTick. home — нейтральный no-op: полки — волна home-feed.
local function moveX(dx)
  if view == "library" then
    local items = rows()
    if #items == 0 then
      return false
    end
    cursor = math.max(1, math.min(#items, cursor + dx))
    clampCursor(items)
    selected = items[cursor]
    return true
  elseif view == "home" then
    -- Горизонтальный ход по карточкам полки home-ленты.
    return homeMoveX(dx)
  end
  return false
end

local function moveY(dy)
  if view == "library" then
    local items = rows()
    if #items == 0 then
      return false
    end
    cursor = math.max(1, math.min(#items, cursor + dy * GRID_COLS))
    clampCursor(items)
    selected = items[cursor]
    return true
  elseif view == "home" then
    -- Вертикальный ход по полкам home-ленты.
    return homeMoveY(dy)
  end
  return false
end

local function setView(id)
  view = id
  cursor = 1
  windowTop = 1
  selected = nil
  searching = false
  -- Смена вида — явный жест пользователя: автоскролл очереди за
  -- играющим снова включён (выключает его ручное j/k в колонке,
  -- подключит волна 2).
  followPlaying = true
  -- Режим ввода живёт ровно пока открыт вид поиска: смена вида любым
  -- путём (Esc, ctrl+j/k, клик по рейлу) обязана вернуть бинды.
  -- Вход в поиск всегда начинает с insert-моды.
  inputMode = id == "search"
  typing = id == "search"
  -- Ввод имени и поповер плейлистов живут только в вью, где открыты:
  -- смена вида (Esc, ctrl+j/k, клик по рейлу) обязана вернуть бинды.
  plNaming = false
  plName = ""
  plAddAfterCreate = nil
  plPicker = nil
  helpShown = false
  -- Home-лента всегда открывается с начала: курсор и окна в ноль.
  if id == "home" then
    homeShelf = 1
    homeItem = 1
    homeTop = 1
    homeLeft = {}
  end
end

local function railNode()
  local items = {}
  for i, v in ipairs(VIEWS) do
    local active = view == v.id or (view == "playlist" and v.id == "library")
    table.insert(items, ui.column({
      key = "rail-" .. v.id,
      width = 48,
      gap = 2,
      align = "center",
      paddingV = 8,
      radius = 6,
      fill = active and COLORS.selected or COLORS.surface,
    }, {
        ui.glyph({ key = "rail-glyph-" .. v.id, name = v.glyph, size = 16, color = active and COLORS.accent or COLORS.muted }),
      lbl(v.title, { key = "rail-label-" .. v.id, size = 10, color = active and COLORS.text or COLORS.muted }),
    }))
  end
  return ui.column({
    key = "rail",
    width = RAIL,
    gap = 8,
    paddingV = PAD,
    fill = COLORS.surface,
    radius = 10,
    align = "center",
  }, items)
end

local function stateNode(s, items)
  if not s.linked then
    return card({
      ui.glyph({ key = "st-glyph", name = "plug-off", color = COLORS.muted }),
      lbl("tmusd is not responding", { key = "st-title", color = COLORS.text }),
      lbl("start it with: systemctl --user start tmusd", { key = "st-hint", color = COLORS.muted }),
    }, { key = "state-card", fill = COLORS.surface })
  end
  if #items > 0 then
    return nil
  end
  local text
  if view == "library" then
    text = "No playlists yet"
  elseif view == "playlist" then
    if tostring(s.tracksId or "") ~= playlistId then
      text = "Loading playlist…"
    else
      text = "No tracks in this playlist"
    end
  elseif view == "clients" then
    text = "No connected clients"
  elseif searching then
    text = "Searching…"
  elseif query == "" then
    text = "Type a query and press Enter"
  else
    text = "Nothing found"
  end
  return card({
    ui.glyph({
      key = "st-glyph",
      name = (searching or (view == "playlist" and tostring(s.tracksId or "") ~= playlistId)) and "loader" or "music",
      color = COLORS.muted,
    }),
    lbl(text, { key = "st-title", color = COLORS.muted }),
  }, { key = "state-card" })
end

local function contextTitle(text)
  return lbl(text, { key = "ctx-title", size = 12, color = COLORS.muted })
end

local function listChildren(slice, first)
  local children = {}
  local lastKind = nil
  if view == "search" and first > 1 then
    local prior = rows()[first - 1]
    lastKind = prior and prior.kind or nil
  end
  for k, row in ipairs(slice) do
    local i = first + k - 1
    if view == "search" and row.kind ~= lastKind then
      table.insert(children, lbl(GROUP_TITLES[row.kind] or row.kind, {
        key = "group-" .. row.kind,
        size = 12,
        color = COLORS.muted,
      }))
      lastKind = row.kind
    end
    if view == "clients" then
      -- Всё из providerMeta: никаких хардкодов имён провайдеров.
      local meta = providerMeta[tostring(row.payload or "")]
      local glyph = providerGlyph(row.payload)
      local authKind = meta and meta.authKind or ""
      local connected = authKind == "Ready" or authKind == "Anonymous"
      local hint = meta and meta.authHint or ""
      -- «All» — агрегатор, а не провайдер: meta у него нет, и «Sign in»
      -- для него соврал бы. Статус нейтральный. Строка перезапуска —
      -- действие, её подпись тоже нейтральная.
      local stateText, stateColor
      if row.restart then
        stateText, stateColor = "Restart", COLORS.muted
      elseif meta == nil then
        stateText, stateColor = "All sources", COLORS.muted
      elseif connected then
        stateText, stateColor = "Connected", COLORS.accent
      else
        stateText, stateColor = "Sign in", COLORS.muted
      end
      table.insert(children, ui.row({
        key = "client" .. i,
        gap = 10,
        padding = 12,
        radius = 6,
        align = "center",
        fill = (i == cursor or row.active) and COLORS.selected or COLORS.surface,
        onClick = function()
          cursor = i
          activate(row)
          draw()
        end,
      }, {
        ui.glyph({
          key = "client-glyph" .. i,
          name = row.active and "circle-check" or "server",
          color = row.active and COLORS.accent or COLORS.muted,
        }),
        lbl(glyph ~= "" and (glyph .. row.title) or row.title, {
          key = "client-title" .. i,
          grow = 1,
          color = COLORS.text,
        }),
        lbl(hint ~= "" and shorten(hint, 40) or row.subtitle, { key = "client-sub" .. i, color = COLORS.muted }),
        lbl(stateText, {
          key = "client-state" .. i,
          color = stateColor,
        }),
      }))
    elseif row.kind == "more" then
      -- Приглашение догрузки: приглушённая, без onClick и event —
      -- курсор на неё может встать, активация ничего не делает.
      table.insert(children, lbl(row.title, {
        key = "more" .. i,
        size = 12,
        color = COLORS.muted,
      }))
    else
      table.insert(children, trackRow(i, row, {
        title = row.title,
        subtitle = row.subtitle,
        art = row.art,
        time = row.time,
        current = row.current,
        rating = row.rating,
        index = row.kind == "client" and "·" or tostring(i),
        event = row.event,
        payload = row.payload,
      }))
    end
  end
  return children
end

-- Хиро home-вида: только типографика и арт — кнопки, прогресс и
-- спектр переехали в npNode, чтобы не дублировать управление в двух
-- местах одного экрана.
local function homeWorkspace(s)
  if not s.linked or (s.title or "") == "" then
    -- Полки/состояния рисует волна 3; без данных вид валиден и пуст.
    return nil
  end
  local dur = tonumber(s.duration) or 0
  local pos = tonumber(s.position) or 0
  local marks = {}
  if s.offline then
    table.insert(marks, "Cached")
  end
  if s.shuffle then
    table.insert(marks, "Shuffled")
  end
  if (s.loop or "none") ~= "none" then
    table.insert(marks, REPEAT_LABEL[tostring(s.loop)] or tostring(s.loop))
  end
  return ui.row({ key = "np-hero", gap = 16, align = "center" }, {
    artNode(s.artPath, 64),
    ui.column({ key = "np-text", gap = 4, flexGrow = 1 }, {
      lbl("Now Playing", { key = "np-over", size = 11, color = COLORS.muted }),
      lbl(providerGlyph(s.provider) .. shorten(tostring(s.title or ""), 40), { key = "np-title", size = 18, color = COLORS.text }),
      lbl(shorten(tostring(s.artist or ""), 40), { key = "np-artist", size = 13, color = COLORS.accent }),
      lbl(
        (tostring(s.album or "") ~= "" and shorten(s.album, 40) .. " · " or "")
          .. clock(pos) .. (dur > 0 and ("/" .. clock(dur)) or "")
          .. (#marks > 0 and (" · " .. table.concat(marks, " · ")) or ""),
        { key = "np-meta", size = 11, color = COLORS.muted }
      ),
    }),
  })
end

-- Карточка-состояние home-ленты: nil/loading, error, empty.
local function homeStateNode(glyph, text)
  return card({
    ui.glyph({ key = "hf-glyph", name = glyph, color = COLORS.muted }),
    lbl(text, { key = "hf-title", color = COLORS.muted }),
  }, { key = "hf-state" })
end

local function homeCardNode(shelfIndex, cardIndex, c)
  local active = shelfIndex == homeShelf and cardIndex == homeItem
  return ui.column({
    key = "hf-card-" .. shelfIndex .. "-" .. cardIndex,
    width = 150,
    gap = 6,
    padding = 8,
    radius = 6,
    fill = active and COLORS.selected or COLORS.surface,
    onClick = function()
      homeShelf = shelfIndex
      homeItem = cardIndex
      draw()
    end,
  }, {
    artNode(c.art, 110),
    lbl(shorten(c.title, 18), { key = "t", color = COLORS.text }),
    lbl(shorten(c.subtitle, 22), { key = "s", color = COLORS.muted }),
  })
end

-- Тело home-ленты: состояние по статусу ключа, иначе видимые полки
-- homeTop..+SHELF_WINDOW-1, каждая — заголовок с честным счётчиком
-- окна и ряд карточек окна homeLeft[полка]..+HOME_CARD_WINDOW-1.
local function homeFeedNode()
  local status = type(homeFeed) == "table" and tostring(homeFeed.status or "") or ""
  if homeFeed == nil or status == "loading" then
    return homeStateNode("loader", "Loading recommendations…")
  end
  if status == "error" then
    local err = type(homeFeed) == "table" and tostring(homeFeed.error or "") or ""
    return homeStateNode("alert", err ~= "" and err or "Recommendations unavailable")
  end
  local shelves = homeShelves()
  if #shelves == 0 then
    return homeStateNode("music", "No recommendations")
  end
  clampHomeCursor()
  local shelfNodes = {}
  local last = math.min(homeTop + SHELF_WINDOW - 1, #shelves)
  for i = homeTop, last do
    local shelf = shelves[i]
    local left = homeLeft[i] or 1
    local header = ui.row({ key = "hf-head-" .. i, gap = 8, align = "center" }, {
      lbl(shelf.title, { key = "hf-shelf-title-" .. i, size = 13, color = COLORS.text }),
      shelf.subtitle ~= "" and lbl(shelf.subtitle, { key = "hf-shelf-sub-" .. i, size = 11, color = COLORS.muted }) or nil,
      lbl("", { key = "hf-spacer-" .. i, grow = 1 }),
      lbl("◄ " .. left .. "–" .. (left + HOME_CARD_WINDOW - 1) .. "/" .. shelf.count .. " ►", {
        key = "hf-range-" .. i,
        size = 10,
        color = COLORS.muted,
      }),
    })
    local cards = {}
    local clast = math.min(left + HOME_CARD_WINDOW - 1, #shelf.cards)
    for j = left, clast do
      table.insert(cards, homeCardNode(i, j, shelf.cards[j]))
    end
    table.insert(shelfNodes, ui.column({ key = "hf-shelf-" .. i, gap = 8 }, {
      header,
      ui.row({ key = "hf-cards-" .. i, gap = GAP }, cards),
    }))
  end
  return ui.scroll({ key = "hf-scroll", flexGrow = 1 }, {
    ui.column({ key = "hf-body", gap = GAP }, shelfNodes),
  })
end

local function playlistCards(slice, first)
  local cards = {}
  for k, row in ipairs(slice) do
    local i = first + k - 1
    table.insert(cards, ui.column({
      key = "pl" .. i,
      width = 168,
      gap = 8,
      padding = 12,
      radius = 6,
      fill = i == cursor and COLORS.selected or COLORS.surface,
      onClick = function()
        cursor = i
        openPlaylist(row)
        draw()
      end,
    }, {
      artNode(row.art, 120),
      lbl(shorten(row.title, 20), { key = "pl-title" .. i, color = COLORS.text }),
      lbl(shorten(row.subtitle, 24), { key = "pl-sub" .. i, color = COLORS.muted }),
    }))
  end
  local grid = {}
  local line = {}
  for i, node in ipairs(cards) do
    table.insert(line, node)
    if #line == 4 or i == #cards then
      table.insert(grid, ui.row({ key = "grid-row" .. #grid, gap = GAP }, line))
      line = {}
    end
  end
  return grid
end

-- Автоскролл очереди за играющим треком: зовётся из refresh() при
-- каждой подмене snapshot. Ручное j/k по колонке (волна 2) выключит
-- followPlaying; Enter на строке и setView вернут его в true.
local lastQueueIndex = nil
local function followQueue()
  local index = tonumber(snapshot.queueIndex)
  if index == lastQueueIndex then
    return
  end
  lastQueueIndex = index
  if not followPlaying or index == nil then
    return
  end
  queueCursor, queueWindowTop = clampScroll(index + 1, index + 1 - 2, #(snapshot.queue or {}), QUEUE_WINDOW)
end

-- Правая колонка-очередь: ЕДИНЫЙ context для всех видов. Рисуется
-- напрямую из snapshot.queue (без фильтра дизлайкнутых — зеркало
-- живой очереди) и со своим окном/курсором; счётчик берём из
-- #snapshot.queue, а не queueLen: state-событие может обгонять массив.
local function queueContextNode(s)
  local queue = s.queue or {}
  clampQueue(queue)
  local index = tonumber(s.queueIndex)
  local playingIdx = index ~= nil and index + 1 or nil
  local children = {
    ui.row({ key = "q-head", gap = 8, align = "center" }, {
      lbl("Queue", {
        key = "q-title",
        size = 12,
        color = focusBuf == 3 and COLORS.accent or COLORS.muted,
      }),
      lbl(queueCursor .. "/" .. #queue, { key = "q-count", size = 11, color = COLORS.muted, grow = 1 }),
    }),
  }
  -- Заголовок фиксирован; строки и пустое состояние живут в скролле,
  -- чтобы длинная очередь не распирала колонку.
  local body
  if #queue == 0 then
    body = ui.column({ key = "q-empty-wrap", gap = GAP }, {
      ui.glyph({ key = "q-empty-glyph", name = "playlist", color = COLORS.muted }),
      lbl("Queue is empty", { key = "q-empty", color = COLORS.muted }),
    })
  else
    local rowsOut = {}
    local last = math.min(queueWindowTop + QUEUE_WINDOW - 1, #queue)
    for i = queueWindowTop, last do
      local t = queue[i]
      local isPlaying = playingIdx ~= nil and i == playingIdx
      -- Подсветка курсора — только при фокусе буфера 3: после
      -- ctrl+h очередь остаётся зеркалом без «селекта-призрака»,
      -- играющий трек подсвечен всегда.
      local isCursor = focusBuf == 3 and i == queueCursor
      local rating = ratingOf(s, idOf(t))
      -- Маркер рейтинга перед названием: ♥ лайк, × дизлайк; нет — ничего.
      local marker = rating == "liked" and "♥ " or (rating == "disliked" and "× " or "")
      table.insert(rowsOut, ui.row({
        key = "q" .. i,
        gap = 8,
        paddingH = 8,
        paddingV = 4,
        radius = 6,
        align = "center",
        fill = isPlaying and COLORS.playing or (isCursor and COLORS.selected or COLORS.bg),
        border = (isCursor and isPlaying) and COLORS.border or nil,
        borderWidth = (isCursor and isPlaying) and 1 or nil,
        -- Клик мышью по строке очереди = намерение работать с ней:
        -- курсор и фокус буфера 3, иначе ввод уйдёт в центральный вид.
        onClick = function()
          queueCursor = i
          followPlaying = false
          focusBuf = 3
          draw()
        end,
      }, {
        ui.row({ key = "q-mark" .. i, width = 24, align = "center" }, {
          lbl(isPlaying and "▶" or tostring(i), {
            key = "q-idx" .. i,
            color = isPlaying and COLORS.accent or COLORS.muted,
          }),
        }),
        -- Бейдж провайдера перед артом: из какого клиента трек.
        ui.row({ key = "q-badge" .. i, width = 22, align = "center" }, {
          lbl(providerGlyph(providerByTrack(t)), { key = "q-badge-t" .. i, color = COLORS.muted }),
        }),
        artNode(t.art_path, 28),
        lbl(shorten(marker .. tostring(t.title or ""), 26), {
          key = "q-title" .. i,
          grow = 1,
          color = isPlaying and COLORS.accent or (isCursor and COLORS.text or COLORS.muted),
        }),
        lbl(durationLabel(t.duration) or "", { key = "q-dur" .. i, width = 44, color = COLORS.muted }),
      }))
    end
    body = ui.column({ key = "q-rows", gap = GAP }, rowsOut)
  end
  table.insert(children, ui.scroll({ key = "q-scroll", flexGrow = 1 }, { body }))
  return ui.column({ key = "queue-context", gap = GAP, flexGrow = 3 }, children)
end

-- Строки вью eq: курсорная — подсветка selected, как в списках.
local function eqWorkspace(s)
  local eq = s.equalizer
  if type(eq) ~= "table" then
    return card({ lbl("eq недоступен", { key = "eq-na", color = COLORS.muted }) }, { key = "eq-card" })
  end
  local lines = {}
  for i, row in ipairs(rows()) do
    local text
    if row.kind == "eq-power" then
      text = "[" .. row.title .. "]"
    elseif row.kind == "eq-preset" then
      text = "[Preset " .. row.title .. "]"
    else
      text = string.format("%-7s %+5.1f  %s", row.freq, row.gain, eqBar(row.gain))
    end
    local active = cursor == i
    table.insert(lines, ui.row({
      key = "eq-line-" .. i,
      gap = 8,
      paddingH = 10,
      paddingV = 4,
      radius = 6,
      fill = active and COLORS.selected or COLORS.surface,
    }, {
      lbl(text, { key = "eq-text-" .. i, grow = 1, color = active and COLORS.text or COLORS.muted }),
    }))
  end
  return ui.column({ key = "eq-lines", gap = 4 }, lines)
end

local function chipNode(kind, title)
  local active = searchKind == kind
  return ui.button({
    key = "chip-" .. kind,
    text = title,
    controlSize = "sm",
    variant = active and "primary" or "ghost",
    onClick = function()
      searchKind = kind
      cursor = 1
      if query ~= "" then
        doSearch()
        return
      end
      draw()
    end,
  })
end

-- Чип фильтра по клиенту: тот же вид, что у чипов вида, но переключение
-- чисто локальное — перевыдача не нужна, выдача уже в snapshot.
local function providerChipNode(pid, title)
  local active = searchProvider == pid
  return ui.button({
    key = "chip-prov-" .. pid,
    text = title,
    controlSize = "sm",
    variant = active and "primary" or "ghost",
    onClick = function()
      searchProvider = pid
      cursor = 1
      draw()
    end,
  })
end

-- c в normal (поиск): цикл All → клиенты по порядку демона → All.
-- Кириллический двойник — физическая C = Cyrillic_es (как A/D-паттерн).
local function cycleSearchProvider()
  local list = { "all" }
  for _, p in ipairs(snapshot.providers or {}) do
    local pid = tostring(p.id or "")
    if pid ~= "" then
      table.insert(list, pid)
    end
  end
  if #list < 2 then
    return
  end
  local idx = 1
  for i, pid in ipairs(list) do
    if pid == searchProvider then
      idx = i
      break
    end
  end
  searchProvider = list[idx % #list + 1]
  cursor = 1
end

-- Ввод имени нового плейлиста: как поиск, буфер ведёт onKey (витрину
-- рисует Library-заголовок). forTrack — трек, который после создания
-- надо сразу добавить.
local function startPlNaming(forTrack)
  plNaming = true
  plName = ""
  plAddAfterCreate = forTrack
end

local function cancelPlNaming()
  plNaming = false
  plName = ""
  plAddAfterCreate = nil
end

local function submitPlNaming()
  local name = string.gsub(plName, ":", "")
  local add = plAddAfterCreate
  cancelPlNaming()
  if name == "" then
    return
  end
  if add then
    -- Создание и добавление — ОДНО составное событие: id нового
    -- плейлиста знает только сервис (он видит stdout `tmus pl new`),
    -- панели он недоступен. Разбор `add:NAME:…` в сервисе по имени
    -- был бы гонкой с дубликатами имён — newadd делает цепочку
    -- детерминированной.
    poke("pl", "newadd:" .. name .. ":" .. add)
  else
    poke("pl", "new:" .. name)
  end
end

-- Трек для «a — в плейлист»: играющий трек (решение хозяина), фолбэк —
-- строка под курсором (нужна kind=="track" и составной id "prov:id").
local function trackForAdd()
  local idstr = tostring(snapshot.trackId or "")
  if idstr ~= "" then
    return idstr
  end
  if view == "home" then
    local shelf = homeShelves()[homeShelf]
    local c = shelf and shelf.cards[homeItem]
    if c and c.kind == "track" then
      return c.id
    end
    return nil
  end
  local row = rows()[cursor]
  if row and row.kind == "track" and row.id then
    return row.id
  end
  return nil
end

-- Поповер «в плейлист»: отдельного оверлей-слоя у panel.render нет,
-- поэтому рисуем карточку выбора вместо правой context-колонки —
-- это самый дешёвый «поповер» без нового API. Состояние в plPicker
-- (track + cursor), пункты = «+ New playlist…» + snapshot.playlists.
local function playlistPickerNode(s)
  local items = { { title = "+ New playlist…" } }
  for _, p in ipairs(s.playlists or {}) do
    local ref = plRef(p)
    local bare = ref:match(":(.+)$") or ref
    table.insert(items, { ref = ref, title = tostring(p.title or ""), lm = bare == "LM" })
  end
  if plPicker.cursor > #items then
    plPicker.cursor = #items
  end
  if plPicker.cursor < 1 then
    plPicker.cursor = 1
  end
  local out = { contextTitle("Add to playlist") }
  for i, item in ipairs(items) do
    local active = i == plPicker.cursor
    table.insert(out, ui.row({
      key = "plpick" .. i,
      gap = 8,
      paddingH = 10,
      paddingV = 6,
      radius = 6,
      align = "center",
      fill = active and COLORS.selected or COLORS.surface,
      onClick = function()
        plPicker.cursor = i
        draw()
      end,
    }, {
      -- LM управляется лайками (edit_playlist туда не пишет) — помечаем
      -- пункт, чтобы выбор читался как лайк, а не добавление.
      lbl(item.lm and (shorten(item.title, 26) .. " · ♥ лайк") or shorten(item.title, 34), {
        key = "plpick-t" .. i,
        grow = 1,
        color = active and COLORS.accent or COLORS.text,
      }),
    }))
  end
  return ui.column({ key = "pl-picker", gap = 6, flexGrow = 3 }, out)
end

-- Строка карты подсказок: клавиша акцентом, действие приглушённым.
local function helpRow(id, keys, text)
  return ui.row({ key = "help-" .. id, gap = 10, align = "center" }, {
    lbl(keys, { key = "help-k-" .. id, width = 190, size = 12, color = COLORS.accent }),
    lbl(text, { key = "help-t-" .. id, grow = 1, size = 12, color = COLORS.muted }),
  })
end

-- Модал подсказок: перекрывает центральные виды (паттерн plPicker),
-- rail/очередь/now-playing остаются на месте — спектр живёт и под
-- модалом. Статичен: одна страница, без скролла и курсора.
local function helpNode()
  local col1 = ui.column({ key = "help-col-nav", gap = 5 }, {
    helpRow("buf", "Tab · ctrl+h/l", "буфер центр ↔ очередь (д · р)"),
    helpRow("views", "ctrl+j/k · 1–5", "секции (о · л)"),
    helpRow("move", "j/k · h/l", "ход: список, сетка; в eq — полоса, ±дБ"),
    helpRow("ends", "g", "первый/последний"),
    helpRow("back", "BackSpace · Esc", "назад: плейлист → Library, поиск → Home, буфер 3 → 2"),
    helpRow("search", "/ · i", "поиск: insert; jj/Esc — normal"),
    helpRow("killword", "ctrl+BackSpace", "удалить слово в запросе"),
    helpRow("prov", "c", "фильтр выдачи по клиенту (поиск)"),
  })
  local col2 = ui.column({ key = "help-col-act", gap = 5 }, {
    helpRow("enter", "Return · Space", "открыть · играть · тумблер eq; пауза"),
    helpRow("track", "n/p · r · s", "трек · repeat · shuffle"),
    helpRow("vol", "< · >", "громкость"),
    helpRow("rate", "f · d", "лайк · дизлайк играющего"),
    helpRow("pl", "a · D", "в плейлист · удалить трек"),
    helpRow("flat", "0", "eq: обнулить полосы (Flat)"),
    helpRow("misc", "o · ctrl+d", "терминал TUI · скрытые плейлисты"),
    helpRow("help", "?", "этот модал (ЙЦУКЕН: Shift+7); закрыть — любая клавиша"),
  })
  return ui.column({ key = "help-modal", gap = GAP, flexGrow = 1 }, {
    lbl("Keymap", { key = "help-title", size = 18, color = COLORS.text }),
    ui.row({ key = "help-cols", gap = GAP, flexGrow = 1 }, { col1, col2 }),
    lbl("любая клавиша — закрыть · медиа-клавиши работают", {
      key = "help-close",
      size = 11,
      color = COLORS.muted,
    }),
  })
end

function draw()
  local s = snapshot
  local items = rows()
  clampCursor(items)
  -- Окно правой колонки-очереди клампится вместе с центральным
  -- списком, чтобы followPlaying/ручной курсор не вылезали за границы.
  clampQueue(s.queue or {})
  local slice, first = visibleWindow(items)

  local wsChildren = nil
  -- Единая правая колонка очереди для всех видов; отдельные context
  -- больше не дублируют её в каждой ветке.
  local context = queueContextNode(s)
  local workspaceGrow = 7
  local contextGrow = 3

  if view == "home" then
    -- Хиро исчезает без трека; npNode внизу остаётся всегда.
    wsChildren = {}
    local hero = homeWorkspace(s)
    if hero then
      table.insert(wsChildren, hero)
    end
    -- Лента рекомендаций: состояния/полки, прокручиваемый скролл.
    table.insert(wsChildren, homeFeedNode())
  elseif view == "search" then
    workspaceGrow = 63
    contextGrow = 37
    local chips = {}
    for _, k in ipairs(SEARCH_KINDS) do
      table.insert(chips, chipNode(k.id, k.title))
    end
    -- Чипы клиентов (§19): строим при >1 подключённом провайдере —
    -- при единственном фильтр вырождается в шум. Глиф + имя клиента.
    local provChips = { providerChipNode("all", "All") }
    for _, p in ipairs(s.providers or {}) do
      local pid = tostring(p.id or "")
      if pid ~= "" then
        local glyph = tostring(p.glyph or "")
        table.insert(provChips, providerChipNode(pid, (glyph ~= "" and glyph .. " " or "") .. tostring(p.name or pid)))
      end
    end
    local state = stateNode(s, items)
    local body = {}
    if state then
      table.insert(body, state)
    else
      table.insert(body, ui.scroll({ key = "results-" .. first, flexGrow = 1 }, {
        ui.column({ key = "results-list", gap = 6 }, listChildren(slice, first)),
      }))
    end
    wsChildren = {
      -- Поле поиска — витрина, а не ui.input: буфер ведёт onKey, а
      -- ui.input после первого рендера игнорирует обновления `value`
      -- (хозяин не видел набираемый текст реал-тайм). Label
      -- перерисовывается каждым draw() — текст живой по определению.
      ui.row({
        key = "search-line",
        gap = 8,
        align = "center",
        paddingH = 12,
        paddingV = 8,
        radius = 8,
        fill = COLORS.surface,
        onClick = function()
          cursor = 1
        end,
      }, {
        ui.glyph({
          key = "search-glyph",
          name = typing and "pencil" or "search",
          size = 14,
          color = typing and COLORS.accent or COLORS.muted,
        }),
        lbl(query ~= "" and query or (typing and "Type…" or "i — ввод · j/k — навигация"), {
          key = "search-text",
          grow = 1,
          color = query ~= "" and COLORS.text or COLORS.muted,
        }),
        lbl(typing and "INSERT" or "NORMAL", {
          key = "search-mode",
          size = 10,
          color = typing and COLORS.accent or COLORS.muted,
        }),
      }),
      ui.row({ key = "chips", gap = 8 }, chips),
      ui.column({ key = "ws-body", gap = GAP, flexGrow = 1 }, body),
    }
    -- Строка чипов клиентов (§19) — между чипами видов и телом выдачи,
    -- только когда есть из чего выбирать (>1 чипа = All + клиенты).
    if #provChips > 1 then
      table.insert(wsChildren, 3, ui.row({ key = "chips-prov", gap = 8 }, provChips))
    end
  elseif view == "library" then
    local state = stateNode(s, items)
    local body = {}
    if state then
      table.insert(body, state)
    else
      table.insert(body, ui.scroll({ key = "library-scroll-" .. first, flexGrow = 1 }, {
        ui.column({ key = "library-grid", gap = GAP }, playlistCards(slice, first)),
      }))
    end
    wsChildren = {
      -- Во время ввода имени заголовок превращается в витрину буфера
      -- (клавиши захвачены, ui.input до них не доходит — грабля из
      -- поиска), подсказка объясняет Enter/Esc.
      plNaming
        and lbl("New playlist: " .. plName .. "▌  (Enter — создать · Esc — отмена)", {
          key = "pl-name-hint",
          size = 14,
          color = COLORS.accent,
        })
        or lbl("Library", { key = "ws-title", size = 18, color = COLORS.text }),
      ui.column({ key = "ws-body", gap = GAP, flexGrow = 1 }, body),
    }
  elseif view == "playlist" then
    local body = {}
    if state then
      table.insert(body, state)
    else
      table.insert(body, ui.scroll({ key = "playlist-scroll-" .. first, flexGrow = 1 }, {
        ui.column({ key = "playlist-list", gap = 4 }, listChildren(slice, first)),
      }))
    end
    wsChildren = {
      lbl(shorten(playlistTitle, 40), { key = "ws-title", size = 18, color = COLORS.text }),
      ui.column({ key = "ws-body", gap = GAP, flexGrow = 1 }, body),
    }
  elseif view == "eq" then
    -- Не через stateNode: linked-демон без поля equalizer дал бы
    -- «Nothing found»; заглушку eq рисует сам eqWorkspace.
    local state = stateNode(s, rows())
    local body = {}
    if state and not s.linked then
      table.insert(body, state)
    else
      table.insert(body, eqWorkspace(s))
    end
    wsChildren = {
      lbl("Equalizer", { key = "ws-title", size = 18, color = COLORS.text }),
      ui.column({ key = "ws-body", gap = GAP, flexGrow = 1 }, body),
    }
  else
    local state = stateNode(s, items)
    local body = {}
    if state then
      table.insert(body, state)
    else
      table.insert(body, ui.scroll({ key = "clients-scroll-" .. first, flexGrow = 1 }, {
        ui.column({ key = "clients-list", gap = 6 }, listChildren(slice, first)),
      }))
    end
    wsChildren = {
      lbl("Clients", { key = "ws-title", size = 18, color = COLORS.text }),
      ui.column({ key = "ws-body", gap = GAP, flexGrow = 1 }, body),
    }
  end

  -- Модал подсказок перекрывает центральные виды в любом буфере —
  -- до npNode: спектр и контролы добавятся ниже и живут под модалом.
  if helpShown then
    wsChildren = { helpNode() }
  end

  -- Нижний now-playing — последний ребёнок workspace-колонки в КАЖДОМ
  -- виде: управление и спектр всегда в одном и том же месте экрана.
  table.insert(wsChildren, npNode(s))
  local workspace = ui.column({ key = "ws", gap = GAP, flexGrow = workspaceGrow }, wsChildren)

  -- Поповер выбора плейлиста перекрывает обычный context в любом виде.
  if plPicker then
    context = playlistPickerNode(s)
  end

  local main = ui.row({
    key = "main",
    gap = GAP,
    flexGrow = 1,
  }, {
    ui.column({ key = "workspace", gap = GAP, flexGrow = workspaceGrow }, { workspace }),
    ui.column({
      key = "context",
      gap = GAP,
      flexGrow = contextGrow,
      padding = PAD,
      radius = 10,
      fill = COLORS.surface,
    }, { context }),
  })

  -- Полная карта клавиш переехала в модал ? (приёмка 19.09: строка
  -- на весь этаж читалась плохо); футер оставляет только два якоря.
  local footer = lbl(
    "? — подсказки · Tab · ctrl+h/l — буферы",
    { key = "footer", size = 11, color = COLORS.muted }
  )

  panel.render(ui.column({
    key = "root",
    gap = GAP,
    padding = PAD,
    fill = COLORS.bg,
    flexGrow = 1,
  }, {
    ui.row({ key = "body", gap = GAP, flexGrow = 1 }, { railNode(), main }),
    footer,
  }))
end

-- Возвращает true, только когда реально изменилось состояние eq
-- (клавиша отправлена демону): зажатые h/l в видах, где они ничего
-- не делают, не должны дёргать draw() каждые ~33 мс (аудит 18.09).
-- Вход в плейлист (l) и выход из него (h) больше не здесь: провал/
-- назад — только Enter/BackSpace, а в сетке Library h/l — это ход
-- moveX (см. onKey и runHeld).
local function handleViewKeys(key)
  if view == "eq" then
    -- h/l в eq не меняют вид: на полосе — ±1 дБ (кламп ±15, панель
    -- шлёт абсолютное значение), на Preset — соседний пресет (с
    -- включением), на Power — вкл/выкл. Перерисовку делает
    -- round-trip состояния.
    local row = rows()[cursor]
    if row and row.kind == "eq-band" then
      local gain = math.max(-EQ_GAIN_LIMIT, math.min(EQ_GAIN_LIMIT, row.gain + (key == "l" and 1 or -1)))
      poke("eq", string.format("band:%d:%g", row.index, gain))
    elseif row and row.kind == "eq-preset" then
      cyclePreset(key == "l" and 1 or -1)
    elseif row and row.kind == "eq-power" then
      poke("eq", key == "l" and "on" or "off")
    end
    return false
  end
  return false
end

-- Темп повтора сведён с системной клавиатурой (config/inputs.lua:
-- repeat_delay 250, repeat_rate 55) — хозяин листает j/k с той же
-- скоростью, что в nvim. Прежние 350/80 были вчетверо медленнее
-- системы и ощущались как залипание. Навигация чуть спокойнее
-- текста (30/с), как в gamepad-launcher: 55 шагов/с проходят всю
-- библиотеку за секунду. Медиа-клавиши медленные сознательно:
-- демон поглощает серию скипов через 120-мс settle, спамить незачем.
local NAV_DELAY_MS = 250
local NAV_REPEAT_MS = 1000 / 30
local DELETE_REPEAT_MS = 1000 / 55
local MEDIA_DELAY_MS = 500
local MEDIA_REPEAT_MS = 500
local MAX_HOLD_MS = 6000

local held = nil
local heldMs = 0
local heldFired = 0

local function startHold(chord, action)
  held = { chord = chord, action = action }
  heldMs = 0
  heldFired = 0
  panel.setNeedsFrameTick(true)
end

local function endHold()
  held = nil
  panel.setNeedsFrameTick(false)
end

local function isMediaAction(action)
  return action == "next" or action == "prev"
end

local function runHeld()
  local action = held.action
  if action == "down" then
    if view == "library" then
      return moveY(1)
    elseif view == "home" then
      return homeMoveY(1)
    end
    move(1)
    return true
  elseif action == "up" then
    if view == "library" then
      return moveY(-1)
    elseif view == "home" then
      return homeMoveY(-1)
    end
    move(-1)
    return true
  elseif action == "delete" then
    -- Удержание BackSpace в поле поиска: темп — как у стрелок
    -- (350 мс разгон, затем каждые 80 мс), а не медиа-клавиш.
    query = string.sub(query, 1, -2)
    return true
  elseif action == "pl-delete" then
    -- Тот же BackSpace, но в буфере имени плейлиста.
    plName = string.sub(plName, 1, -2)
    return true
  elseif action == "h" or action == "l" then
    if view == "eq" then
      -- Прежняя логика ±дБ / соседний пресет.
      return handleViewKeys(action)
    elseif view == "library" then
      -- Горизонтальный ход по сетке карточек.
      return moveX(action == "l" and 1 or -1)
    elseif view == "home" then
      -- Горизонтальный ход по карточкам полки home-ленты.
      return homeMoveX(action == "l" and 1 or -1)
    end
    return false
  elseif action == "q-down" then
    moveQueue(1)
    return true
  elseif action == "q-up" then
    moveQueue(-1)
    return true
  elseif action == "next" then
    poke("next")
    return false
  elseif action == "prev" then
    poke("prev")
    return false
  elseif action == "vol-down" then
    poke("vol-down")
    return false
  elseif action == "vol-up" then
    poke("vol-up")
    return false
  end
  return false
end

function onFrameTick(deltaMs)
  if not held then
    return
  end
  heldMs = heldMs + (deltaMs or 0)
  if heldMs >= MAX_HOLD_MS then
    endHold()
    return
  end
  local delay = isMediaAction(held.action) and MEDIA_DELAY_MS or NAV_DELAY_MS
  local interval = isMediaAction(held.action) and MEDIA_REPEAT_MS
    or (held.action == "delete" and DELETE_REPEAT_MS or NAV_REPEAT_MS)
  if heldMs < delay then
    return
  end
  local due = math.floor((heldMs - delay) / interval) + 1
  local owed = due - heldFired
  if owed <= 0 then
    return
  end
  heldFired = due
  local needsDraw = false
  for _ = 1, owed do
    if runHeld() then
      needsDraw = true
    end
  end
  if needsDraw then
    draw()
  end
end

function onKey(chord, pressed)
  if not pressed then
    if held and held.chord == chord then
      endHold()
    end
    return
  end

  if held and held.chord == chord then
    return
  end

  local key = LAYOUT[chord] or chord

  if chord == "ctrl+j" or chord == "ctrl+Cyrillic_o" or chord == "ctrl+k" or chord == "ctrl+Cyrillic_el" then
    local forward = chord == "ctrl+j" or chord == "ctrl+Cyrillic_o"
    local section = view == "playlist" and "library" or view
    local i = VIEW_IDS[section] or 1
    i = i + (forward and 1 or -1)
    if i < 1 then
      i = #VIEWS
    elseif i > #VIEWS then
      i = 1
    end
    setView(VIEWS[i].id)
    draw()
    return
  end

  if chord == "XF86AudioNext" or chord == "XF86AudioPrev" then
    local action = chord == "XF86AudioNext" and "next" or "prev"
    poke(action)
    startHold(chord, action)
    return
  end

  -- Ввод имени плейлиста: тот же принцип, что и поиск — клавиши
  -- захвачены хостом, буфер ведём сами, витрина в заголовке Library.
  if plNaming then
    if chord == "Return" then
      submitPlNaming()
    elseif chord == "Escape" then
      cancelPlNaming()
    elseif chord == "BackSpace" or chord == "Backspace" then
      plName = string.sub(plName, 1, -2)
      startHold(chord, "pl-delete")
    elseif chord == "Space" then
      if #plName < 64 then
        plName = plName .. " "
      end
    else
      local ch = TEXT_CHARS[chord]
      if ch and #plName < 64 then
        plName = plName .. ch
      end
    end
    draw()
    return
  end

  -- Поповер выбора плейлиста: j/k — навигация, Enter — добавить (или
  -- уйти в ввод имени), Esc — отмена. Всё локально, демона не дёргаем
  -- до подтверждения.
  if plPicker then
    local count = 1 + #(snapshot.playlists or {})
    if chord == "Escape" then
      plPicker = nil
    elseif chord == "Return" then
      if plPicker.cursor == 1 then
        local track = plPicker.track
        plPicker = nil
        startPlNaming(track)
      else
        local pl = (snapshot.playlists or {})[plPicker.cursor - 1]
        if pl then
          local ref = plRef(pl)
          local bare = ref:match(":(.+)$") or ref
          if bare == "LM" then
            -- Liked Music нельзя править через edit_playlist — он
            -- целиком выводится из лайков; подтверждение на LM = лайк
            -- трека, ради которого и открыт пикер.
            poke("rate", "like:" .. plPicker.track)
          else
            poke("pl", "add:" .. ref .. ":" .. plPicker.track)
          end
        end
        plPicker = nil
      end
    elseif key == "j" or chord == "Down" then
      plPicker.cursor = plPicker.cursor % count + 1
    elseif key == "k" or chord == "Up" then
      plPicker.cursor = (plPicker.cursor - 2) % count + 1
    end
    draw()
    return
  end

  -- Модал подсказок (? — вопросик в обеих раскладках: ЙЦУКЕН —
  -- Shift+7, keysym question). Открываем только из normal: в insert
  -- поиска ввод есть ввод. Закрытие — любая клавиша (до карт ниже,
  -- чтобы j/k не листали список за модалом); медиа-клавиши и
  -- ctrl+j/k секций обрабатываются выше и работают сквозь модал,
  -- смена вида закрывает его в setView.
  if helpShown then
    helpShown = false
    draw()
    return
  end
  -- ? — keysym question в обеих раскладках, но хост может отдать
  -- хорду с модификатором (Shift+question) или в другом регистре
  -- (прецедент BackSpace/Backspace) — принимаем все формы через
  -- lower. Буквы хост сглаживает в верхний регистр, пунктуацию —
  -- как повезёт; capture_keys держит обе записи (plugin.toml).
  local lq = string.lower(tostring(chord))
  if (lq == "question" or lq == "shift+question") and not typing then
    helpShown = true
    draw()
    return
  end

  -- Фокус буферов 2↔3: Tab и ctrl+h/ctrl+l — тумглер двух колонок
  -- (центральный список ↔ очередь, в обе стороны). Смена фокуса гасит
  -- insert тем же принципом, что setView — «смена контекста любым
  -- путём обязана вернуть бинды». Фокус не трогает view/cursor/
  -- windowTop: возврат в буфер 2 возвращает курсор на место.
  -- Кириллические двойники ЙЦУКЕН: физическая l = Cyrillic_de («д»),
  -- физическая h = Cyrillic_er («р») — зеркало ctrl+Cyrillic_o/el
  -- для j/k. Хорды сравниваем после string.lower: хост отдаёт имена
  -- в смешанном регистре (прецедент двойного написания BackSpace).
  local lc = string.lower(chord)
  if lc == "tab" or lc == "ctrl+l" or lc == "ctrl+h" or lc == "ctrl+cyrillic_de" or lc == "ctrl+cyrillic_er" then
    typing = false
    insertPending = nil
    focusBuf = (focusBuf == 3) and 2 or 3
    draw()
    return
  end

  -- Поиск: вим-модальность. typing=true — insert: буквы идут в
  -- запрос, Esc или быстрые пары jj/jk/oo (физические j/k/o; на
  -- ЙЦУКЕН это о/л/о/о) выходят в normal. typing=false — normal:
  -- j/k и стрелки листают выдачу, Enter активирует строку, i/слэш/
  -- точка возвращают insert. Медиа-клавиши работают в обеих.
  -- Поиск живёт только при фокусе буфера 2: в буфере 3 (очередь)
  -- буквы и хорды идут в его карту, а не в запрос.
  if inputMode and focusBuf == 2 then
    local isJ = chord == "J" or chord == "Cyrillic_o"
    local isK = chord == "K" or chord == "Cyrillic_el"
    local isO = chord == "O" or chord == "Cyrillic_shcha"
    if typing then
      -- Детект эскейп-пары: первый символ уже вставлен в запрос —
      -- при подтверждении пары он снимается. Окно 250 мс, как
      -- repeat_delay системной клавиатуры.
      if insertPending and (noctalia.nowMs() - insertPending.at) <= INSERT_ESCAPE_MS then
        local starter = insertPending.key
        if (starter == "j" and (isJ or isK)) or (starter == "o" and isO) then
          query = string.sub(query, 1, -2)
          typing = false
          insertPending = nil
          draw()
          return
        end
      end
      insertPending = nil
      if chord == "Return" then
        -- Enter двоякий, как в каждом лаунчере: изменившийся запрос
        -- уходит в поиск, неизменный (выдача уже на экране) —
        -- активирует выбранную строку. Обе ветки выводят в normal:
        -- витрина INSERT/NORMAL не должна рассинхронизироваться.
        if query ~= "" and query ~= lastSearched then
          doSearch()
        else
          activate(rows()[cursor])
        end
        typing = false
        insertPending = nil
      elseif chord == "Escape" then
        typing = false
      elseif lc == "ctrl+backspace" then
        -- Удалить слово (см. killWord). Без автоповтора сознательно:
        -- лаг одного кадра съел бы полстроки.
        query = killWord(query)
      elseif chord == "BackSpace" or chord == "Backspace" then
        query = string.sub(query, 1, -2)
        startHold(chord, "delete")
      elseif chord == "Up" then
        move(-1)
        startHold(chord, "up")
      elseif chord == "Down" then
        move(1)
        startHold(chord, "down")
      elseif chord == "Space" then
        if #query < 64 then
          query = query .. " "
        end
      else
        local ch = TEXT_CHARS[chord]
        if ch and #query < 64 then
          query = query .. ch
          if isJ or isO then
            insertPending = { key = isJ and "j" or "o", at = noctalia.nowMs() }
          end
        end
      end
    else
      if chord == "Return" then
        activate(rows()[cursor])
        -- Активация выводит в normal (витрина не должна врать):
        -- возврат в insert — только i/slash/period ниже.
        typing = false
        insertPending = nil
      elseif chord == "Escape" then
        setView("home")
      elseif key == "i" or chord == "slash" or chord == "period" then
        typing = true
        insertPending = nil
      elseif chord == "C" or chord == "c" or chord == "Cyrillic_es" then
        -- c — фильтр выдачи по клиенту (§19), как `c` в TUI сужает
        -- источник каталога; в insert-режиме это просто буква.
        -- Оба регистра латиницы: хост отдаёт по-разному (прецедент
        -- f-бинда нижним и A-бинда верхним регистром).
        cycleSearchProvider()
      elseif key == "j" then
        move(1)
        startHold(chord, "down")
      elseif key == "k" then
        move(-1)
        startHold(chord, "up")
      elseif chord == "Up" then
        move(-1)
        startHold(chord, "up")
      elseif chord == "Down" then
        move(1)
        startHold(chord, "down")
      elseif key == "g" then
        local count = #rows()
        cursor = cursor == 1 and count or 1
        maybeLoadMore()
      elseif chord == "BackSpace" or chord == "Backspace" then
        -- Назад на home — зеркало Esc; захват остаётся, чтобы
        -- клавиша не утекала под панель.
        setView("home")
      end
    end
    draw()
    return
  end

  -- Буфер 3 — правая колонка-очередь. Собственная карта хода/оценки/
  -- активации; Escape и нераспознанные клавиши (n/p, r, s, </>,
  -- ctrl+d, o, цифры) НЕ перехватываются — проваливаются в глобальную
  -- карту ниже, как в буфере 2.
  if focusBuf == 3 then
    local handled = true
    if key == "j" or chord == "Down" then
      moveQueue(1)
      startHold(chord, "q-down")
    elseif key == "k" or chord == "Up" then
      moveQueue(-1)
      startHold(chord, "q-up")
    elseif key == "g" then
      local queue = snapshot.queue or {}
      queueCursor = queueCursor == 1 and #queue or 1
      clampQueue(queue)
    elseif chord == "Return" then
      -- Активация строки очереди: 0-базный индекс, сервис уже мапит
      -- на CLI queue-goto. Жест пользователя — автоскролл за играющим
      -- снова включён.
      poke("queue-goto", tostring(queueCursor - 1))
      followPlaying = true
    elseif chord == "Space" then
      poke("toggle")
    elseif chord == "BackSpace" or chord == "Backspace" then
      -- Назад в центральный буфер — зеркало фокус-тумглера выше.
      focusBuf = 2
    elseif key == "f" then
      rateQueueRow("liked")
    elseif key == "d" then
      rateQueueRow("disliked")
    elseif chord == "A" or chord == "Cyrillic_f" then
      -- a: играющий трек — в поповер «в плейлист»; фолбэк — строка
      -- курсора очереди (решение хозяина: цель f/d/a — играющий).
      local track = tostring(snapshot.trackId or "")
      if track == "" then
        track = idOf((snapshot.queue or {})[queueCursor])
      end
      if track ~= "" then
        plPicker = { track = track, cursor = 1 }
      end
    elseif chord == "D" or chord == "Cyrillic_v" then
      -- no-op: в протоколе нет удаления из очереди — глотаем, чтобы
      -- не провалиться в глобальную карту (там D удаляет из плейлиста).
    else
      handled = false
    end
    if handled then
      draw()
      return
    end
    -- Нераспознанное — в глобальную карту ниже.
  end

  if chord == "Escape" then
    panel.close()
    return
  elseif chord == "Return" then
    -- home-лента: активация карточки под курсором (трек — play,
    -- плейлист — открытие вида playlist). Прочие виды — строка списка.
    if view == "home" and focusBuf == 2 then
      homeActivate()
    else
      activate(rows()[cursor])
    end
  elseif chord == "Space" then
    if view == "playlist" or view == "eq" then
      activate(rows()[cursor])
    else
      poke("toggle")
    end
  elseif key == "n" then
    poke("next")
  elseif key == "p" then
    poke("prev")
  elseif key == "j" then
    if view == "library" or view == "home" then
      moveY(1)
    else
      move(1)
    end
    startHold(chord, "down")
  elseif chord == "Down" then
    if view == "library" or view == "home" then
      moveY(1)
    else
      move(1)
    end
    startHold(chord, "down")
  elseif key == "k" then
    if view == "library" or view == "home" then
      moveY(-1)
    else
      move(-1)
    end
    startHold(chord, "up")
  elseif chord == "Up" then
    if view == "library" or view == "home" then
      moveY(-1)
    else
      move(-1)
    end
    startHold(chord, "up")
  elseif key == "less" then
    -- Громкость с автоповтором, как у j/k: service обслуживает
    -- vol-down/vol-up шагом ±5. Кейсим угловой скобки — «less»/«greater»
    -- (голые «<»/«>» хост capture_keys не принимает).
    poke("vol-down")
    startHold(chord, "vol-down")
  elseif key == "greater" then
    poke("vol-up")
    startHold(chord, "vol-up")
  elseif key == "h" or key == "l" or chord == "Left" or chord == "Right" then
    if chord == "Left" then
      key = "h"
    elseif chord == "Right" then
      key = "l"
    end
    if view == "eq" then
      -- Прежняя логика eq: ±дБ / соседний пресет (handleViewKeys).
      handleViewKeys(key)
    else
      -- Горизонтальный ход (сетка Library; в остальных видах —
      -- клампнутый ±1 по rows()).
      moveX(key == "l" and 1 or -1)
    end
    startHold(chord, key)
  elseif chord == "1" or chord == "2" or chord == "3" or chord == "4" or chord == "5" or chord == "6" then
    -- Цифра за границей VIEWS (например «6» после удаления queue-вида)
    -- не должна давать setView(nil).
    local n = tonumber(chord)
    if n and n >= 1 and n <= #VIEWS then
      setView(VIEWS[n].id)
    end
  elseif key == "0" and view == "eq" then
    -- Flat не трогает enabled: выключенный эквалайзер с обнулёнными
    -- полосами — ровно то, что ожидает «0».
    poke("eq", "flat")
  elseif key == "g" then
    -- g — «концы»: в списках прыжок первый/последний; в home-ленте —
    -- полный сброс курсора и окон (g на полке-карточке списка не в
    -- фокусе: home всегда буфер 2).
    if view == "home" then
      homeShelf = 1
      homeItem = 1
      homeTop = 1
      homeLeft = {}
    else
      local count = #rows()
      cursor = cursor == 1 and count or 1
      maybeLoadMore()
    end
  elseif key == "r" then
    poke("repeat", REPEAT_CYCLE[tostring(snapshot.loop or "none")] or "none")
  elseif key == "s" then
    poke("shuffle", "toggle")
  elseif key == "f" then
    -- Оценка строки курсора (или текущего трека на home); повторное
    -- нажатие снимает оценку — см. rateSelected.
    rateSelected("liked")
  elseif key == "d" then
    rateSelected("disliked")
  elseif chord == "A" or chord == "Cyrillic_f" then
    -- a: Library — ввод имени нового плейлиста; в списках треков —
    -- поповер «в плейлист». chord приходит в верхнем регистре (сглаживает
    -- хост), ЛАТИНИЦА: физическая «a» на ЙЦУКЕН — это Cyrillic_f.
    -- В виде поиска 'a' недоступна: там insert-режим съедает буквы.
    if view == "library" then
      startPlNaming(nil)
    else
      local track = trackForAdd()
      if track then
        plPicker = { track = track, cursor = 1 }
      end
    end
  elseif chord == "D" or chord == "Cyrillic_v" then
    -- D (shift+d): удаление. Прямой chord проверяем ДО LAYOUT: таблица
    -- мапит заглавные в нижний регистр, и D→"d" столкнулся бы с
    -- rateSelected("disliked"). В плейлисте — убрать трек под курсором,
    -- в Library на карточке плейлиста — удалить сам плейлист.
    -- pullLibrary() на стороне сервиса перезакажет список, панель
    -- обновится через watch KEY "tmus".
    if view == "playlist" then
      local row = rows()[cursor]
      local ref
      if row and row.kind == "track" and row.id then
        for _, pl in ipairs(snapshot.playlists or {}) do
          if idOf(pl) == playlistId then
            ref = plRef(pl)
            break
          end
        end
        if ref then
          poke("pl", "rm:" .. ref .. ":" .. row.id)
        end
      end
    elseif view == "library" then
      local row = rows()[cursor]
      if row and row.kind == "playlist" and (row.plid or "") ~= "" then
        poke("pl", "del:" .. row.plid)
      end
    end
  elseif chord == "ctrl+d" then
    -- Тогл показа дизлайкнутых: фильтрует rows() локально, демона
    -- не дёргает. Ловится только здесь (не в insert-режиме поиска),
    -- как и все остальные бинды.
    showHidden = not showHidden
  elseif key == "o" then
    poke("tui")
    panel.close()
  elseif key == "slash" or chord == "slash" or chord == "period" then
    -- `period` — та же физическая клавиша на ЙЦУКЕН: точка входит в
    -- поиск и без переключения раскладки.
    setView("search")
  elseif chord == "BackSpace" then
    -- Назад по иерархии видов: из плейлиста — в Library (бывший h),
    -- из search-normal — на home (бывший no-op, зеркало Esc; в
    -- search-insert BackSpace обслуживает поле в inputMode-блоке
    -- выше, сюда попадает только не-input случай — ветка оставлена
    -- как защита). Прочее — глотаем, чтобы не утекала в приложения
    -- под панелью.
    if view == "playlist" then
      setView("library")
    elseif view == "search" and not inputMode then
      setView("home")
    end
  end

  draw()
end

local function refresh(state)
  snapshot = state or {}
  if lastSearchSerial == nil or snapshot.searchSerial ~= lastSearchSerial then
    searching = false
  end
  followQueue()
  draw()
end

-- Транзиентные модалы (поповер «в плейлист», ввод имени нового плейлиста
-- вместе с взведённым `plAddAfterCreate`, карта клавиш, «двойной ввод»)
-- обязаны умирать вместе с панелью. Причина: закрытие панели может пройти
-- МИМО ветки `onKey` (Esc забирает хост, клик по бару, `noctalia msg
-- panel-close`), и тогда состояние переживало закрытие: на следующем
-- открытии человек снова видел тот же поповер, а «+ New playlist…» ещё и
-- держал трек наготове. Воспроизведено 21.09.2026 обвязкой panel.luau:
-- `A` → Esc → переоткрытие показывало поповер заново («Esc не отменяет
-- действие»).
local function clearTransient()
  plPicker = nil
  plNaming = false
  plName = ""
  plAddAfterCreate = nil
  helpShown = false
  insertPending = nil
end

function onOpen()
  -- Открытие панели — полный сброс навигации: фокус в центральный
  -- список, очередь с начала, автоскролл за играющим включён.
  clearTransient()
  focusBuf = 2
  queueCursor = 1
  queueWindowTop = 1
  followPlaying = true
  poke("refresh")
  refresh(noctalia.state.get(KEY))
end

function onClose()
  endHold()
  -- Страховка: закрытие посреди drag слайдера не должно оставлять
  -- устаревший черновик громкости следующему открытию панели.
  volDraft = nil
  -- Тот же принцип для модалов: закрытие = отмена (Esc в поповере может
  -- до `onKey` не дойти, а незакрытый `plAddAfterCreate` — это отложенное
  -- действие с чужим треком).
  clearTransient()
end

function doSearch()
  lastSearchSerial = snapshot.searchSerial
  lastSearched = query
  searching = true
  poke("search", (SEARCH_KIND_TOKENS[searchKind] or "all") .. "|" .. noctalia.string.urlEncode(query))
  draw()
end

function submitSearch(text)
  query = noctalia.string.trim(text or "")
  if query == "" then
    return
  end
  setView("search")
  doSearch()
end

noctalia.state.watch(KEY, refresh)
-- Лёгкий ключ "tmus.now" (1 Гц): точечно обновляем волатильные поля
-- текущего snapshot, тяжёлый snapshot не заменяем (аудит 18.09).
noctalia.state.watch("tmus.now", function(now)
  if type(now) ~= "table" or type(snapshot) ~= "table" then
    return
  end
  snapshot.linked = now.linked
  snapshot.status = now.status
  snapshot.position = now.position
  snapshot.duration = now.duration
  snapshot.volume = now.volume
  draw()
end)
refresh(noctalia.state.get(KEY))
-- Спектр: лёгкий ключ от виджета, троттлинг ≥125 мс (8 Гц) — на
-- стороне виджета, панель не считает и не рисует чаще, чем приходят
-- значения; тяжёлый snapshot KEY "tmus" не трогаем (аудит 18.09).
-- nil/пустая строка — бары пропадают до следующего значения.
noctalia.state.watch("tmus.spectrum", function(v)
  spectrum = (type(v) == "string" and v ~= "") and v or nil
  draw()
end)
-- Home-лента рекомендаций: лёгкий ключ "tmus.home" (сервис публикует
-- status + полки; art_path декорирован у первых 14 карточек полки).
-- Рендер-модель полок перевычисляется ТОЛЬКО здесь — draw до 8 Гц
-- перебирать полки не должен. Сервис обязан быть обработчиком
-- onUpdateState: обновляем кэш и перерисовываем.
noctalia.state.watch("tmus.home", function(v)
  homeFeed = type(v) == "table" and v or nil
  rebuildHomeShelves()
  if view == "home" then
    clampHomeCursor()
  end
  draw()
end)
