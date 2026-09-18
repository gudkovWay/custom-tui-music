--!nonstrict

local KEY = "tmus"
local SERVICE = "q/tmus:watch"

-- Цвета — ИМЕНА РОЛЕЙ палитры хоста, а не hex: хост сам резолвит их в
-- актуальный Material-цвет, поэтому смена темы/обоев перекрашивает
-- панель без правок плагина. Роли проверены живой пробой 19.09:
-- surface/surface_variant/hover/surface_container_high/
-- secondary_container/tertiary/on_surface/on_surface_variant/outline/
-- primary/error рендерятся, а primary_container валиден, но НЕ
-- рисуется (невидим) — его здесь сознательно нет. Играющая строка
-- (playing) и курсорная (selected) могут численно совпасть
-- (surface_container_high ≈ hover) — их различие держит не заливка, а
-- маркер ▶ и accent-цвет текста играющего трека.
local COLORS = {
  bg = "surface",
  surface = "surface_variant",
  -- Курсорная строка обязана читаться с одного взгляда: проба 18.09
  -- показала, что прежний хозяин не видел её.
  selected = "hover",
  playing = "surface_container_high",
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
local CARD_WINDOW = 12

local VIEWS = {
  { id = "home", title = "Home", glyph = "home" },
  { id = "search", title = "Search", glyph = "search" },
  { id = "library", title = "Library", glyph = "library" },
  { id = "queue", title = "Queue", glyph = "playlist" },
  { id = "clients", title = "Clients", glyph = "server" },
  { id = "eq", title = "Equalizer", glyph = "sliders" },
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
local query = ""
local lastSearched = nil
-- Вим-модальность поля поиска: typing=true — insert (буквы идут в
-- запрос), false — normal (j/k листают, Enter активирует). Выход из
-- insert: Esc или пары jj/jk/oo на физических клавишах (о/л на
-- ЙЦУКЕН — это те же j/k); вход: i, / или точка.
local typing = false
local insertPending = nil
local INSERT_ESCAPE_MS = 250
local searchKind = "all"
local lastSearchSerial = nil
local doSearch -- defined below; used by chips and submitSearch
local rows -- defined below; used by rateSelected (cursor row lookup)

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
  local idx = ((at - 1 + step) % #EQ_PRESET_NAMES) + 1
  poke("eq", "preset:" .. EQ_PRESET_NAMES[idx])
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
local function spectrumBars(count, maxHeight, barWidth, gap)
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
      fill = COLORS.accent,
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

-- Ссылка на плейлист для IPC "pl": БЕЗ двоеточий. Сервис разбирает
-- add/rm как "^([^:]+):([^:]+):(.+)$" — составной idOf вида "prov:id"
-- сломал бы разбор, поэтому передаём голый id (CLI находит плейлист
-- сам). По той же причине имя плейлиста после create чистим от ':'.
local function plRef(p)
  if type(p.id) == "table" then
    return tostring(p.id.id or "")
  end
  return tostring(p.id or "")
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

-- Оценка трека под курсором; на home (Now-Playing) — текущего трека.
-- Повторное f/d на той же оценке снимает её: шлём "none". Панель
-- сверяется со своим snapshot: актуальность гарантирует событие
-- rating_changed, обновляющее snapshot.ratings раньше клавиши.
local function rateSelected(want)
  local idstr
  if view == "home" then
    idstr = tostring(snapshot.trackId or "")
  else
    local row = rows()[cursor]
    if row and row.kind == "track" and row.id then
      idstr = row.id
    end
  end
  if idstr == nil or idstr == "" then
    return
  end
  local send = (ratingOf(snapshot, idstr) == want) and "none" or want
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
    padding = PAD,
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

local function currentTrack(s)
  if not s.linked or (s.title or "") == "" then
    return nil
  end
  local dur = tonumber(s.duration) or 0
  local pos = tonumber(s.position) or 0
  local time = dur > 0 and (clock(pos) .. " / " .. clock(dur)) or clock(pos)
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
  local rows = {
    ui.row({ key = "ct-top", gap = 10, align = "center" }, {
      artNode(s.artPath, 48),
      ui.column({ key = "ct-text", gap = 2, flexGrow = 1 }, {
        lbl(shorten(tostring(s.title or ""), 34), { key = "ct-title", size = 14, color = COLORS.accent }),
        lbl(shorten(tostring(s.artist or ""), 34), { key = "ct-artist", color = COLORS.muted }),
      }),
    }),
    lbl(time .. (#marks > 0 and ("  ·  " .. table.concat(marks, " · ")) or ""), {
      key = "ct-time",
      color = COLORS.muted,
    }),
  }
  if dur > 0 then
    table.insert(rows, ui.progress({
      key = "ct-progress",
      progress = math.max(0, math.min(1, pos / dur)),
      flexGrow = 1,
      height = 4,
      radius = 2,
    }))
  end
  local loopMode = tostring(s.loop or "none")
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
      -- Три различимых состояния: track — «repeat-once», queue — «repeat»,
      -- оба с подсветкой primary; none — тусклый «repeat».
      glyph = loopMode == "track" and "repeat-once" or "repeat",
      controlSize = "sm",
      variant = loopMode ~= "none" and "primary" or "ghost",
      onClick = function()
        poke("repeat", REPEAT_CYCLE[tostring(s.loop or "none")] or "none")
      end,
    }),
  }
  if loopMode ~= "none" then
    table.insert(controls, ui.label(REPEAT_SHORT[loopMode] or loopMode, {
      key = "ct-repeat-short",
      size = 11,
      color = COLORS.muted,
    }))
  end
  table.insert(rows, ui.row({ key = "ct-controls", gap = 8, align = "center" }, controls))
  return card(rows, { key = "current-card" })
end

local function sourcePill(p, active)
  return ui.button({
    key = "src-" .. p.id,
    text = p.title,
    controlSize = "sm",
    variant = active and "primary" or "ghost",
    onClick = function()
      poke("source", p.id)
    end,
  })
end

local function sourceSelector(s)
  local pills = {
    sourcePill({ id = "all", title = "All" }, tostring(s.source or "all") == "all"),
  }
  for _, p in ipairs(s.providers or {}) do
    table.insert(pills, sourcePill({ id = p.id, title = p.name }, tostring(s.source or "all") == p.id))
  end
  return ui.row({ key = "sources", gap = 8, align = "center" }, pills)
end

local function trackRow(i, t, opts)
  local o = opts or {}
  local active = i == cursor
  local current = o.current
  -- Маркер рейтинга перед названием: ♥ лайк, × дизлайк; нет — ничего.
  local marker = o.rating == "liked" and "♥ " or (o.rating == "disliked" and "× " or "")
  local children = {
    ui.box({ key = "mark" .. i, width = 28 }, {
      lbl(current and "▶" or (o.index or tostring(i)), {
        key = "idx" .. i,
        color = current and COLORS.accent or COLORS.muted,
      }),
    }),
    artNode(o.art, 32),
    lbl(shorten(marker .. o.title, 52), {
      key = "title" .. i,
      grow = 1,
      color = current and COLORS.accent or (active and COLORS.text or COLORS.muted),
    }),
    lbl(shorten(o.subtitle, 32), { key = "sub" .. i, color = COLORS.muted, maxWidth = 220 }),
    lbl(o.time or "", { key = "time" .. i, color = COLORS.muted, width = 56 }),
  }
  -- Мини-спектр у играющей строки: 7 полос до 8 px в блоке ~44 px.
  -- Только для current и только пока виден спектр — иначе строки
  -- списков не меняют геометрию при скролле.
  if current and spectrum then
    table.insert(children, ui.box({ key = "spec-mini" .. i, width = 44 }, {
      spectrumBars(7, 8, 4, 2),
    }))
  end
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
        poke(o.event, o.payload)
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
    if (want == "all" or want == kind) and GROUP_TITLES[kind] then
      local row = resultRow(r)
      row.kind = kind
      -- Фильтр дизлайков касается только треков-результатов.
      local hidden = row.kind == "track" and hiddenByRating(s, row.id or "")
      if not hidden then
        table.insert(out, row)
        if #out >= 200 then
          break
        end
      end
    end
  end
  return out
end

function rows()
  local s = snapshot
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
        if i > 200 then
          break
        end
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
    end
  elseif view == "queue" then
    local index = tonumber(s.queueIndex)
    for i, t in ipairs(s.queue or {}) do
      if i > 200 then
        break
      end
      local idstr = idOf(t)
      if not hiddenByRating(s, idstr) then
        table.insert(out, {
          kind = "track",
          id = idstr,
          rating = ratingOf(s, idstr),
          title = tostring(t.title or ""),
          subtitle = type(t.artists) == "table" and table.concat(t.artists, ", ") or "",
          art = tostring(t.art_path or ""),
          event = "play",
          payload = idstr,
          time = durationLabel(t.duration),
          current = index ~= nil and i == index + 1 or false,
        })
      end
    end
  elseif view == "search" then
    return searchRows(s)
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

  return out
end

local function windowSize()
  return view == "library" and CARD_WINDOW or LIST_WINDOW
end

local function clampCursor(items)
  local count = #items
  if count == 0 then
    cursor = 1
    windowTop = 1
    return
  end
  if cursor < 1 then
    cursor = 1
  elseif cursor > count then
    cursor = count
  end
  local maxTop = math.max(1, count - windowSize() + 1)
  if windowTop < 1 then
    windowTop = 1
  elseif windowTop > maxTop then
    windowTop = maxTop
  end
  if cursor < windowTop then
    windowTop = cursor
  elseif cursor >= windowTop + windowSize() then
    windowTop = cursor - windowSize() + 1
  end
  if windowTop > maxTop then
    windowTop = maxTop
  end
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

local function activate(row)
  if row == nil then
    return
  end
  selected = row
  if row.kind == "eq-preset" then
    -- Имя берём из живого snapshot: между рендером строки и нажатием
    -- пресет мог смениться (h/l листает соседний).
    local eq = snapshot.equalizer
    if type(eq) == "table" then
      poke("eq", "preset:" .. tostring(eq.preset or ""))
    end
    return
  end
  if row.open then
    openPlaylist(row)
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
end

local function setView(id)
  view = id
  cursor = 1
  windowTop = 1
  selected = nil
  searching = false
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
  if view == "home" then
    text = "Nothing playing"
  elseif view == "library" then
    text = "No playlists yet"
  elseif view == "playlist" then
    if tostring(s.tracksId or "") ~= playlistId then
      text = "Loading playlist…"
    else
      text = "No tracks in this playlist"
    end
  elseif view == "queue" then
    text = "Queue is empty"
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
        lbl(row.title, { key = "client-title" .. i, grow = 1, color = COLORS.text }),
        lbl(row.subtitle, { key = "client-sub" .. i, color = COLORS.muted }),
        lbl("Connected", { key = "client-state" .. i, color = COLORS.muted }),
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

local function homeWorkspace(s)
  if not s.linked or (s.title or "") == "" then
    return stateNode(s, {})
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
  -- Громкость приезжает по лёгкому ключу "tmus.now", так что чип живой.
  table.insert(marks, "vol " .. math.floor(tonumber(s.volume) or 0) .. "%")
  local children = {
    ui.row({ key = "np-hero", gap = 20, align = "center" }, {
      artNode(s.artPath, 200),
      ui.column({ key = "np-text", gap = 8, flexGrow = 1 }, {
        lbl("Now Playing", { key = "np-over", size = 12, color = COLORS.muted }),
        lbl(shorten(tostring(s.title or ""), 34), { key = "np-title", size = 26, color = COLORS.text }),
        lbl(shorten(tostring(s.artist or ""), 40), { key = "np-artist", size = 16, color = COLORS.accent }),
        -- Спектр в хиро: ~20 полос до 28 px, рисуется только пока
        -- виджет публикует "tmus.spectrum" (nil — тихо пропускаем).
        spectrum and spectrumBars(20, 28) or nil,
        lbl((tostring(s.album or "") ~= "" and shorten(s.album, 40) .. "  ·  " or "")
          .. clock(pos) .. (dur > 0 and (" / " .. clock(dur)) or "")
          .. (#marks > 0 and ("  ·  " .. table.concat(marks, " · ")) or ""), {
          key = "np-meta",
          color = COLORS.muted,
        }),
      }),
    }),
  }
  local npButtons = {
    ui.button({
      key = "prev",
      glyph = "player-skip-back",
      controlSize = "md",
      variant = "ghost",
      onClick = function()
        poke("prev")
      end,
    }),
    ui.button({
      key = "toggle",
      glyph = s.status == "playing" and "player-pause" or "player-play",
      controlSize = "md",
      variant = "primary",
      onClick = function()
        poke("toggle")
      end,
    }),
    ui.button({
      key = "next",
      glyph = "player-skip-forward",
      controlSize = "md",
      variant = "ghost",
      onClick = function()
        poke("next")
      end,
    }),
    ui.button({
      key = "shuffle",
      glyph = "arrows-shuffle",
      controlSize = "md",
      variant = s.shuffle and "primary" or "ghost",
      onClick = function()
        poke("shuffle", "toggle")
      end,
    }),
    ui.button({
      key = "repeat",
      -- Три различимых состояния, как в карточке current: track —
      -- «repeat-once», queue — «repeat», оба primary; none — ghost.
      glyph = (tostring(s.loop or "none")) == "track" and "repeat-once" or "repeat",
      controlSize = "md",
      variant = (tostring(s.loop or "none")) ~= "none" and "primary" or "ghost",
      onClick = function()
        poke("repeat", REPEAT_CYCLE[tostring(s.loop or "none")] or "none")
      end,
    }),
  }
  if tostring(s.loop or "none") ~= "none" then
    table.insert(npButtons, ui.label(REPEAT_SHORT[tostring(s.loop or "none")] or tostring(s.loop), {
      key = "np-repeat-short",
      size = 11,
      color = COLORS.muted,
    }))
  end
  local controls = ui.row({ key = "np-controls", gap = 10, align = "center" }, npButtons)
  if dur > 0 then
    table.insert(children, ui.progress({
      key = "np-progress",
      progress = math.max(0, math.min(1, pos / dur)),
      flexGrow = 1,
      height = 5,
      radius = 2,
    }))
  end
  table.insert(children, controls)
  return card(children, { key = "now-playing", grow = 1, gap = 16 })
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
      artNode(row.art, 144),
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

local function homeContext(s)
  local children = {
    ui.button({
      key = "home-search",
      glyph = "search",
      text = "Search",
      controlSize = "sm",
      variant = "ghost",
      onClick = function()
        -- Раньше это было ui.input, но Enter перехватывается хостом
        -- (capture_keys) и до submit не доходил — поле было мёртвым.
        -- Ввод живёт в виде поиска.
        setView("search")
      end,
    }),
    contextTitle("Queue"),
  }
  local queue = snapshot.queue or {}
  local index = tonumber(snapshot.queueIndex)
  local shown = 0
  for i, t in ipairs(queue) do
    if shown >= 5 then
      break
    end
    if index == nil or i > index + 1 then
      shown = shown + 1
      table.insert(children, ui.row({
        key = "home-q" .. i,
        gap = 8,
        paddingH = 8,
        paddingV = 4,
        radius = 6,
        align = "center",
        onClick = function()
          poke("play", idOf(t))
        end,
      }, {
        lbl(shorten(tostring(t.title or ""), 26), { key = "home-qt" .. i, grow = 1, color = COLORS.text }),
        lbl(durationLabel(t.duration) or "", { key = "home-qd" .. i, color = COLORS.muted }),
      }))
    end
  end
  if shown == 0 then
    table.insert(children, lbl("Queue is empty", { key = "home-q-empty", color = COLORS.muted }))
  end
  return ui.column({ key = "home-context", gap = GAP, flexGrow = 3 }, children)
end

local function searchContext(items)
  local children = { contextTitle("Selected") }
  local row = items[cursor] or selected
  if row then
    table.insert(children, card({
      ui.row({ key = "sel-top", gap = 10, align = "center" }, {
        artNode(row.art, 48, row.kind == "playlist" and "playlist" or (row.kind == "artist" and "user" or "music")),
        ui.column({ key = "sel-text", gap = 2, flexGrow = 1 }, {
          lbl(shorten(row.title, 34), { key = "sel-title", color = COLORS.text }),
          lbl(GROUP_TITLES[row.kind] or row.kind, { key = "sel-kind", color = COLORS.muted }),
        }),
      }),
      lbl(shorten(row.subtitle, 40), { key = "sel-sub", color = COLORS.muted }),
    }, { key = "selected-card", fill = COLORS.selected, gap = 8 }))
  else
    table.insert(children, lbl("Nothing selected", { key = "sel-none", color = COLORS.muted }))
  end
  local current = currentTrack(snapshot)
  if current then
    table.insert(children, contextTitle("Current"))
    table.insert(children, current)
  end
  return ui.column({ key = "search-context", gap = GAP, flexGrow = 37 }, children)
end

local function playlistContext(s)
  local heroArt = playlistArtPath
  for _, pl in ipairs(s.playlists or {}) do
    if idOf(pl) == playlistId then
      heroArt = tostring(pl.art_path or "")
    end
  end
  local children = {
    card({
      ui.row({ key = "hero", gap = 12, align = "center" }, {
        artNode(heroArt, 72, "playlist"),
        ui.column({ key = "hero-text", gap = 2 }, {
          lbl("Playlist", { key = "hero-over", size = 12, color = COLORS.muted }),
          lbl(shorten(playlistTitle, 30), { key = "hero-title", size = 16, color = COLORS.text }),
        }),
      }),
    }, { key = "hero-card", gap = 8 }),
  }
  local current = currentTrack(s)
  if current then
    table.insert(children, contextTitle("Current"))
    table.insert(children, current)
  end
  local index = tonumber(s.queueIndex)
  local upNext = 0
  local list = { contextTitle("Up Next") }
  for i, t in ipairs(s.queue or {}) do
    if index == nil or i > index + 1 then
      upNext = upNext + 1
      if upNext <= 5 then
        table.insert(list, ui.row({
          key = "up" .. i,
          gap = 8,
          paddingH = 8,
          paddingV = 4,
          radius = 6,
          align = "center",
          onClick = function()
            poke("play", idOf(t))
          end,
        }, {
          lbl(shorten(tostring(t.title or ""), 26), { key = "up-t" .. i, grow = 1, color = COLORS.text }),
          lbl(durationLabel(t.duration) or "", { key = "up-d" .. i, color = COLORS.muted }),
        }))
      end
    end
  end
  if upNext > 0 then
    for _, node in ipairs(list) do
      table.insert(children, node)
    end
  end
  return ui.column({ key = "playlist-context", gap = GAP, flexGrow = 3 }, children)
end

local function queueContext(s)
  local children = { contextTitle("Current") }
  local current = currentTrack(s)
  if current then
    table.insert(children, current)
  else
    table.insert(children, lbl("Nothing playing", { key = "q-none", color = COLORS.muted }))
  end
  return ui.column({ key = "queue-context", gap = GAP, flexGrow = 3 }, children)
end

local function clientsContext(s)
  local children = {
    contextTitle("Current source"),
    card({
      lbl(tostring(s.source or "all") == "all" and "All clients" or tostring(s.source), {
        key = "cl-source",
        color = COLORS.text,
      }),
      lbl("Library and search use this source", { key = "cl-hint", color = COLORS.muted }),
    }, { key = "cl-card", gap = 6 }),
    contextTitle("Switch source"),
  }
  table.insert(children, sourceSelector(s))
  return ui.column({ key = "clients-context", gap = GAP, flexGrow = 3 }, children)
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

-- Трек для «a — в плейлист»: на home — играющий, иначе строка под
-- курсором (нужна kind=="track" и составной id "prov:id").
local function trackForAdd()
  if view == "home" then
    local idstr = tostring(snapshot.trackId or "")
    if idstr ~= "" then
      return idstr
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
    table.insert(items, { ref = plRef(p), title = tostring(p.title or "") })
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
      lbl(shorten(item.title, 34), {
        key = "plpick-t" .. i,
        grow = 1,
        color = active and COLORS.accent or COLORS.text,
      }),
    }))
  end
  return ui.column({ key = "pl-picker", gap = 6, flexGrow = 3 }, out)
end

function draw()
  local s = snapshot
  local items = rows()
  clampCursor(items)
  local slice, first = visibleWindow(items)

  local workspace = nil
  local context = nil
  local workspaceGrow = 7
  local contextGrow = 3

  if view == "home" then
    workspace = ui.column({ key = "ws", gap = GAP, flexGrow = 7 }, {
      sourceSelector(s),
      homeWorkspace(s),
    })
    context = homeContext(s)
  elseif view == "search" then
    workspaceGrow = 63
    contextGrow = 37
    local chips = {}
    for _, k in ipairs(SEARCH_KINDS) do
      table.insert(chips, chipNode(k.id, k.title))
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
    workspace = ui.column({ key = "ws", gap = GAP, flexGrow = 63 }, {
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
    })
    context = searchContext(items)
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
    workspace = ui.column({ key = "ws", gap = GAP, flexGrow = 7 }, {
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
    })
  elseif view == "playlist" then
    local state = stateNode(s, items)
    local body = {}
    if state then
      table.insert(body, state)
    else
      table.insert(body, ui.scroll({ key = "playlist-scroll-" .. first, flexGrow = 1 }, {
        ui.column({ key = "playlist-list", gap = 4 }, listChildren(slice, first)),
      }))
    end
    workspace = ui.column({ key = "ws", gap = GAP, flexGrow = 7 }, {
      lbl(shorten(playlistTitle, 40), { key = "ws-title", size = 18, color = COLORS.text }),
      ui.column({ key = "ws-body", gap = GAP, flexGrow = 1 }, body),
    })
    context = playlistContext(s)
  elseif view == "queue" then
    local state = stateNode(s, items)
    local body = {}
    if state then
      table.insert(body, state)
    else
      table.insert(body, ui.scroll({ key = "queue-scroll-" .. first, flexGrow = 1 }, {
        ui.column({ key = "queue-list", gap = 4 }, listChildren(slice, first)),
      }))
    end
    workspace = ui.column({ key = "ws", gap = GAP, flexGrow = 7 }, {
      lbl("Queue", { key = "ws-title", size = 18, color = COLORS.text }),
      ui.column({ key = "ws-body", gap = GAP, flexGrow = 1 }, body),
    })
    context = queueContext(s)
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
    workspace = ui.column({ key = "ws", gap = GAP, flexGrow = 7 }, {
      lbl("Equalizer", { key = "ws-title", size = 18, color = COLORS.text }),
      ui.column({ key = "ws-body", gap = GAP, flexGrow = 1 }, body),
    })
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
    workspace = ui.column({ key = "ws", gap = GAP, flexGrow = 7 }, {
      lbl("Clients", { key = "ws-title", size = 18, color = COLORS.text }),
      ui.column({ key = "ws-body", gap = GAP, flexGrow = 1 }, body),
    })
    context = clientsContext(s)
  end

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

  local footer = lbl(
    view == "eq"
      and "j/k move · h/l adjust · Return toggle · 0 flat · 1-6 views"
      or "ctrl+j/k views · 1–6 views · j/k move · Return open · Space play/pause · h/l back/deep · "
        .. "/ search (i insert · jj/Esc normal) · n/p track · r repeat · s shuffle · < /> vol · "
        .. "f/d rate · ctrl+d hidden · o terminal · g ends · a в плейлист · D удалить",
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

-- Возвращает true, только когда вид реально сменился (вход в плейлист
-- или выход из него): зажатые h/l в видах, где они ничего не делают,
-- не должны дёргать draw() каждые ~33 мс (аудит 18.09).
local function handleViewKeys(key)
  if view == "eq" then
    -- h/l в eq не меняют вид: на полосе — ±1 дБ (кламп ±15, панель
    -- шлёт абсолютное значение), на Preset — соседний пресет, на
    -- Power — ничего. Перерисовку делает round-trip состояния.
    local row = rows()[cursor]
    if row and row.kind == "eq-band" then
      local gain = math.max(-EQ_GAIN_LIMIT, math.min(EQ_GAIN_LIMIT, row.gain + (key == "l" and 1 or -1)))
      poke("eq", string.format("band:%d:%g", row.index, gain))
    elseif row and row.kind == "eq-preset" then
      cyclePreset(key == "l" and 1 or -1)
    end
    return false
  end
  if key == "l" then
    if view == "library" then
      local row = rows()[cursor]
      if row and row.open then
        openPlaylist(row)
        return true
      end
    end
  elseif key == "h" then
    if view == "playlist" then
      setView("library")
      return true
    end
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
    move(1)
    return true
  elseif action == "up" then
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
    return handleViewKeys(action)
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
          poke("pl", "add:" .. plRef(pl) .. ":" .. plPicker.track)
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

  -- Поиск: вим-модальность. typing=true — insert: буквы идут в
  -- запрос, Esc или быстрые пары jj/jk/oo (физические j/k/o; на
  -- ЙЦУКЕН это о/л/о/о) выходят в normal. typing=false — normal:
  -- j/k и стрелки листают выдачу, Enter активирует строку, i/слэш/
  -- точка возвращают insert. Медиа-клавиши работают в обеих.
  if inputMode then
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
        -- активирует выбранную строку.
        if query ~= "" and query ~= lastSearched then
          doSearch()
        else
          activate(rows()[cursor])
        end
      elseif chord == "Escape" then
        typing = false
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
      elseif chord == "Escape" then
        setView("home")
      elseif key == "i" or chord == "slash" or chord == "period" then
        typing = true
        insertPending = nil
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
      elseif chord == "BackSpace" or chord == "Backspace" then
        -- Захвачена, чтобы не утекала под панель; в normal не делает
        -- ничего.
      end
    end
    draw()
    return
  end
  if chord == "Escape" then
    panel.close()
    return
  elseif chord == "Return" then
    activate(rows()[cursor])
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
    move(1)
    startHold(chord, "down")
  elseif chord == "Down" then
    move(1)
    startHold(chord, "down")
  elseif key == "k" then
    move(-1)
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
    handleViewKeys(key)
    startHold(chord, key)
  elseif chord == "1" or chord == "2" or chord == "3" or chord == "4" or chord == "5" or chord == "6" then
    setView(VIEWS[tonumber(chord)].id)
  elseif key == "0" and view == "eq" then
    -- Flat не трогает enabled: выключенный эквалайзер с обнулёнными
    -- полосами — ровно то, что ожидает «0».
    poke("eq", "flat")
  elseif key == "g" then
    local count = #rows()
    cursor = cursor == 1 and count or 1
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
    -- Захвачена, чтобы не утекала в приложения под панелью; в поиске
    -- обрабатывает поле, вне его — гасим.
  end

  draw()
end

local function refresh(state)
  snapshot = state or {}
  if lastSearchSerial == nil or snapshot.searchSerial ~= lastSearchSerial then
    searching = false
  end
  draw()
end

function onOpen()
  poke("refresh")
  refresh(noctalia.state.get(KEY))
end

function onClose()
  endHold()
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
