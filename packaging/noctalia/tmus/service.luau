--!nonstrict

local KEY = "tmus"

local TMUS = noctalia.commandExists("tmus") and "tmus"
  or noctalia.expandPath("~/.cargo/bin/tmus")

local RETRY_MS = 3000
local CALL_MS = 15000
local LIBRARY_MS = 300000
local HOME_MS = 300000

local data = {
  linked = false,
  error = "",
  status = "stopped",
  title = "",
  artist = "",
  album = "",
  provider = "",
  artUrl = "",
  artPath = "",
  position = 0,
  duration = 0,
  volume = 0,
  loop = "none",
  shuffle = false,
  offline = false,
  lastError = "",
  source = "all",
  providers = {},
  playlists = {},
  ratings = {},
  trackId = "",
  queue = {},
  queueIndex = nil,
  results = {},
  tracks = {},
  tracksId = "",
  -- Хвост плейлиста: true/nil — догрузка возможна, false — демон
  -- отдал пустую страницу (дочитано). Начальное true: пока не
  -- доказано обратное, за потолком первой страницы могут быть треки.
  hasMore = true,
  tracksTitle = "",
  searchSerial = 0,
  at = 0,
}

local streaming = false
local retryAt = 0
local libraryAt = 0
local sourceReady = false
-- Летящая догрузка хвоста плейлиста: повторный tracks-more, пока не
-- вернулся предыдущий call, игнорируется. Сбрасывается в cb и onDone.
local tracksMoreInFlight = false
-- Летящая догрузка home-ленты: тот же принцип, что у tracks-more.
-- Сбрасывается также в pullHome — новая лента обнуляет продолжение,
-- и застрявший guard не должен блокировать следующую догрузку.
local homeMoreInFlight = false

-- Метка жизни потока `tmus events`: любая строка (включая keepalive)
-- двигает её. У runStream нет onClose, поэтому единственный след
-- умершего потока — тишина дольше интервала keepalive (2 с); 6 с
-- покрывает три пропущенных метки и не задевает паузу.
local STALE_MS = 6000
local lastLineAt = noctalia.nowMs()

local function urlDecode(text)
  return (tostring(text):gsub("%%(%x%x)", function(hex)
    return string.char(tonumber(hex, 16))
  end))
end

local function seconds(value)
  if type(value) == "table" then
    return (tonumber(value.secs) or 0) + ((tonumber(value.nanos) or 0) / 1000000000)
  end
  return tonumber(value) or 0
end

local artDirty = false

-- Лёгкий ключ для подписок на 1 Гц: позиция/статус приходят каждую
-- секунду, а маршалить из-за них всю очередь/результаты/плейлисты —
-- пустая работа (аудит 18.09). Тяжёлый и лёгкий ключи публикуются
-- всегда парой, кроме ветки position — там достаточно лёгкого.
local NOW_KEY = "tmus.now"
local HOME_KEY = "tmus.home"
-- Лента Home: shelves — загруженные полки, next — курсор продолжения
-- демона (nil/"" — дочитано). Статус loading/error/ok как раньше.
local homeState = { status = "idle", shelves = {}, next = nil }
local homeAt = 0
local homeDirty = false

local function publishHome()
  noctalia.state.set(HOME_KEY, homeState)
end

local function publishNow()
  data.at = os.time()
  noctalia.state.set(NOW_KEY, {
    linked = data.linked,
    status = data.status,
    position = data.position,
    duration = data.duration,
    -- Громкость волатильна и меняется чаще всего; в панели её чип в
    -- Now-Playing обновляется по лёгкому ключу без перечитывания
    -- тяжёлого снапшота.
    volume = data.volume,
    at = data.at,
  })
end

local function publish()
  data.at = os.time()
  noctalia.state.set(KEY, data)
  publishNow()
end

local artCache = { pending = {}, done = {} }
local artDir = nil

local function safeKey(text)
  return (tostring(text):gsub("[^%w%-_]", "_"))
end

local function artFile(category, provider, key)
  if not artDir then
    local dir = noctalia.pluginDataDir()
    if type(dir) ~= "string" or dir == "" then
      return nil
    end
    artDir = dir .. "/art"
    noctalia.mkdirAll(artDir)
  end
  return artDir .. "/" .. safeKey(category) .. "-" .. safeKey(provider) .. "-" .. safeKey(key) .. ".img"
end

local function objectArtFile(category, obj)
  local id = type(obj) == "table" and obj.id or nil
  if type(id) == "table" then
    return artFile(category, id.provider or "", id.id or "")
  end
  return artFile(category, "", id or "")
end

local function ensureArt(url, path, onReady)
  if type(url) ~= "string" or url == "" then
    onReady(nil)
    return
  end
  local resolved = artCache.done[url]
  if resolved ~= nil then
    onReady(resolved)
    return
  end
  local waiters = artCache.pending[url]
  if waiters then
    table.insert(waiters, onReady)
    return
  end
  if path and noctalia.fileExists(path) then
    artCache.done[url] = path
    onReady(path)
    return
  end
  artCache.pending[url] = { onReady }
  local accepted = noctalia.download(url, path, function(ok)
    local queued = artCache.pending[url] or {}
    artCache.pending[url] = nil
    artCache.done[url] = ok and path or false
    for _, fn in ipairs(queued) do
      fn(ok and path or nil)
    end
  end)
  if not accepted then
    artCache.pending[url] = nil
    artCache.done[url] = false
    onReady(nil)
  end
end

local function decorateArt(category, obj)
  if type(obj) ~= "table" then
    return
  end
  local url = type(obj.art_url) == "string" and obj.art_url or ""
  if url == "" then
    obj.art_path = ""
    return
  end
  local path = objectArtFile(category, obj)
  if not path then
    obj.art_path = ""
    return
  end
  obj.art_path = noctalia.fileExists(path) and path or ""
  ensureArt(url, path, function(local_path)
    if obj.art_url == url then
      obj.art_path = local_path or ""
      artDirty = true
    end
  end)
end

-- Асинхронные арты ленты Home идут в отдельный ключ tmus.home;
-- republish тяжёлого ключа ради них не нужен — homeDirty
-- обрабатывает update(). Сам decorateArt не трогаем: им живут
-- очередь, поиск и библиотека.
local function decorateHomeArt(category, obj)
  if type(obj) ~= "table" then
    return
  end
  local url = type(obj.art_url) == "string" and obj.art_url or ""
  if url == "" then
    obj.art_path = ""
    return
  end
  local path = objectArtFile(category, obj)
  if not path then
    obj.art_path = ""
    return
  end
  obj.art_path = noctalia.fileExists(path) and path or ""
  ensureArt(url, path, function(local_path)
    if obj.art_url == url then
      obj.art_path = local_path or ""
      homeDirty = true
    end
  end)
end

-- Арты ленты: общее для начальной загрузки и догрузок home-more.
-- Качаем только окно первых 14 карточек полки — панель рендерит окно,
-- хвост полки не держит.
local function decorateHomeShelves(shelves)
  for _, shelf in ipairs(shelves) do
    local items = type(shelf.items) == "table" and shelf.items or {}
    for i, item in ipairs(items) do
      if i > 14 then
        break
      end
      if item.kind == "track" then
        decorateHomeArt("track", item)
      elseif item.kind == "playlist" then
        decorateHomeArt("playlist", item)
      end
    end
  end
end

-- Ключ карточки ленты для дедупликации: kind + составной id. Карточка
-- без id (или с пустым id) ключа не имеет — nil: дедуп её не трогает,
-- каждый экземпляр попадает в ленту (комментарию выше не противоречит).
local function homeCardKey(item)
  local id = type(item) == "table" and item.id or nil
  local kind = tostring(type(item) == "table" and item.kind or "")
  local prov, bare
  if type(id) == "table" then
    prov = tostring(id.provider or "")
    bare = tostring(id.id or "")
  else
    bare = tostring(id or "")
  end
  if prov == nil and bare == "" then
    return nil
  end
  if prov ~= nil and prov == "" and bare == "" then
    return nil
  end
  return kind .. ":" .. (prov ~= nil and (prov .. ":") or "") .. bare
end

-- Детерминированное слияние страницы home-more в загруженную ленту.
-- Полка матчится по title+subtitle: совпала — дополняется карточками,
-- которых ещё НЕ БЫЛО во всей ленте (глобальный набор seen: одна и та
-- же карточка в двух полках дважды не рисуется); новая полка —
-- дописывается в конец в порядке демона. Существующие полки не
-- переупорядочиваются — экран не прыгает под курсором.
local function mergeHomeShelves(oldShelves, page)
  local shelves = {}
  local byTitle = {}
  for _, s in ipairs(oldShelves) do
    table.insert(shelves, s)
    byTitle[tostring(s.title or "") .. "|" .. tostring(s.subtitle or "")] = s
  end
  local seen = {}
  for _, s in ipairs(shelves) do
    for _, it in ipairs(type(s.items) == "table" and s.items or {}) do
      local ck = homeCardKey(it)
      -- Ключа нет (карточка без id) — в seen не пишем: такие карточки
      -- не дедуплицируются и всегда проходят.
      if ck ~= nil then
        seen[ck] = true
      end
    end
  end
  local pages = type(page.shelves) == "table" and page.shelves or {}
  for _, ns in ipairs(pages) do
    local key = tostring(ns.title or "") .. "|" .. tostring(ns.subtitle or "")
    local target = byTitle[key]
    if target == nil then
      target = {}
      for k, v in pairs(ns) do
        target[k] = v
      end
      target.items = {}
      byTitle[key] = target
      table.insert(shelves, target)
    end
    for _, it in ipairs(type(ns.items) == "table" and ns.items or {}) do
      local ck = homeCardKey(it)
      if ck == nil or not seen[ck] then
        if ck ~= nil then
          seen[ck] = true
        end
        table.insert(target.items, it)
      end
    end
  end
  return shelves
end

local function trackFields(track)
  if type(track) ~= "table" then
    return "", "", "", "", ""
  end
  local id = track.id
  local provider = type(id) == "table" and tostring(id.provider or "") or ""
  local artists = type(track.artists) == "table" and table.concat(track.artists, ", ") or ""
  local album = type(track.album) == "string" and track.album or ""
  local art = type(track.art_url) == "string" and track.art_url or ""
  return tostring(track.title or ""), artists, album, provider, art
end

local function applyTrack(track)
  data.title, data.artist, data.album, data.provider, data.artUrl = trackFields(track)
  -- Строка "provider:id" для биндов f/d панели: текущий трек в
  -- Now-Playing не лежит ни в одном списке, панели нужен его id.
  local tid = type(track) == "table" and track.id or nil
  if type(tid) == "table" then
    data.trackId = tostring(tid.provider or "") .. ":" .. tostring(tid.id or "")
  else
    data.trackId = ""
  end
  if type(track) == "table" then
    data.duration = seconds(track.duration)
  else
    data.duration = 0
  end
  data.artPath = ""
  local url = data.artUrl
  if url == "" then
    return
  end
  local path = objectArtFile("track", track)
  if not path then
    return
  end
  data.artPath = noctalia.fileExists(path) and path or ""
  ensureArt(url, path, function(local_path)
    if data.artUrl == url then
      data.artPath = local_path or ""
      publish()
    end
  end)
end

local function applyState(state)
  if type(state) ~= "table" then
    return
  end
  data.status = tostring(state.status or "stopped")
  data.position = seconds(state.position)
  data.volume = tonumber(state.volume) or 0
  data.loop = tostring(state.loop_mode or "none")
  data.shuffle = state.shuffle == true
  data.offline = state.offline == true
  -- last_error шлёт только новый демон; старый молчит — поэтому or "",
  -- как у equalizer выше.
  data.lastError = tostring(state.last_error or "")
  data.queueLen = tonumber(state.queue_len) or 0
  data.queueIndex = tonumber(state.queue_index)
  -- Эквалайзер опционален: старый демон шлёт состояние без поля —
  -- тогда панель показывает заглушку, а не nil-ошибку.
  if type(state.equalizer) == "table" then
    data.equalizer = state.equalizer
  end
  applyTrack(state.track)
  if data.duration == 0 then
    data.duration = seconds(state.duration)
  end
end

local function call(args, onOk, onDone, disconnectOnError)
  local argv = { TMUS }
  for _, a in ipairs(args) do
    table.insert(argv, a)
  end
  noctalia.runAsync(argv, function(res)
    if not res or res.exitCode ~= 0 then
      local why = res and noctalia.string.trim(res.stderr or "") or ""
      data.error = why ~= "" and why or "tmus не ответил"
      if disconnectOnError == true then
        data.linked = false
      end
      if onDone then
        onDone()
      end
      publish()
      return
    end
    data.error = ""
    data.linked = true
    if onOk then
      onOk(noctalia.json.decode(res.stdout or ""))
    end
    if onDone then
      onDone()
    end
    publish()
  end, CALL_MS)
end

local function poke(args)
  call(args, nil)
end

local function pullStatus()
  call({ "status", "--json" }, function(got)
    applyState(got)
  end, nil, true)
end

-- Нормализация capabilities провайдера: список строк или объект
-- {playlist_create=..,playlist_add=..,...} → множество имён.
local function capsFrom(raw)
  local out = {}
  if type(raw) ~= "table" then
    return out
  end
  if raw[1] ~= nil then
    for _, name in ipairs(raw) do
      out[tostring(name)] = true
    end
  else
    for name, on in pairs(raw) do
      if on == true then
        out[tostring(name)] = true
      end
    end
  end
  return out
end

local function pullProviders()
  call({ "providers", "--json" }, function(got)
    local out = {}
    for _, p in ipairs(type(got) == "table" and got or {}) do
      local auth = p.auth
      local kind = type(auth) == "table" and tostring(next(auth) or "") or tostring(auth or "")
      -- Берём ВСЕХ провайдеров: неавторизованный тоже должен быть виден
      -- в Clients (там по authKind показываем Sign in).
      local hint = ""
      if type(auth) == "table" and type(auth[kind]) == "table" then
        hint = tostring(auth[kind].hint or "")
      end
      table.insert(out, {
        id = tostring(p.id or ""),
        name = tostring(p.name or p.id or ""),
        glyph = tostring(p.glyph or ""),
        color = tostring(p.color or ""),
        authKind = kind,
        authHint = hint,
        -- Возможности провайдера (snapshotCapabilities): плейлистные
        -- операции и rate. Снимок может прийти списком строк или
        -- объектом имя->bool — принимаем обе формы; отсутствие поля
        -- (старый демон) = пустой набор: панель показывает локальные
        -- адресаты, нативные прячет.
        caps = capsFrom(p.capabilities),
      })
    end
    data.providers = out
  end)
end

local function pullLibrary()
  libraryAt = noctalia.nowMs()
  call({ "library", "--provider", data.source, "--json" }, function(got)
    data.playlists = type(got) == "table" and got or {}
    for _, pl in ipairs(data.playlists) do
      decorateArt("playlist", pl)
    end
  end)
end

local function pullHome()
  homeAt = noctalia.nowMs()
  -- Новая лента обнуляет продолжение: застрявший guard догрузки не
  -- должен переживать перезагрузку (путь reset/error).
  homeMoreInFlight = false
  homeState = { status = "loading", shelves = homeState.shelves, next = nil }
  publishHome()
  call({ "home", "--provider", data.source, "--json" }, function(got)
    -- Home возвращается ОБЪЕКТОМ { shelves, next }: shelves — полки
    -- страницы, next — курсор продолжения (nil/"" — дочитано).
    local page = type(got) == "table" and got or {}
    local shelves = type(page.shelves) == "table" and page.shelves or {}
    decorateHomeShelves(shelves)
    local next = type(page.next) == "string" and page.next ~= "" and page.next or nil
    homeState = { status = "ok", shelves = shelves, next = next, at = os.time() }
  end, function()
    -- onDone дёргается и после успешного onOk: ошибкой считаем
    -- только случай, когда статус остался "loading".
    if homeState.status == "loading" then
      homeState.status = "error"
    end
    publishHome()
  end)
end

-- Догрузка home-ленты по курсору продолжения: панель стучится, когда
-- курсор подошёл к концу загруженных полок. Повторный запрос, пока
-- летит предыдущий, схлопывает homeMoreInFlight; cb/onDone его снимают.
local function pullHomeMore(cursor)
  if homeMoreInFlight or homeState.status ~= "ok" then
    return
  end
  if type(homeState.next) ~= "string" or homeState.next == "" then
    return
  end
  -- Панель шлёт зеркало homeState.next; протухший курсор (лента уже
  -- перезагружена/дочитана) отвергаем явно, а не игнорируем аргумент.
  if tostring(cursor or "") ~= homeState.next then
    return
  end
  homeMoreInFlight = true
  local want = homeState.next
  call({ "home-more", want, "--json" }, function(got)
    homeMoreInFlight = false
    -- Пока летел запрос, ленту перезагрузили (pullHome): страница
    -- чужая — вписывать её в новую ленту нельзя.
    if homeState.status ~= "ok" or homeState.next ~= want then
      return
    end
    local page = type(got) == "table" and got or {}
    local shelves = mergeHomeShelves(homeState.shelves, page)
    decorateHomeShelves(shelves)
    homeState.shelves = shelves
    homeState.next = type(page.next) == "string" and page.next ~= "" and page.next or nil
    homeState.at = os.time()
  end, function()
    homeMoreInFlight = false
    publishHome()
  end)
end

local function pullRatings()
  -- Payload::Ratings — массив пар [{provider,id},"liked"|...]; "none"
  -- в список не попадает: отсутствие оценки ключа не имеет.
  call({ "ratings", "--json" }, function(got)
    local out = {}
    for _, pair in ipairs(type(got) == "table" and got or {}) do
      local tid, rating = pair[1], pair[2]
      if type(tid) == "table" and type(rating) == "string" and rating ~= "none" then
        out[tostring(tid.provider or "") .. ":" .. tostring(tid.id or "")] = rating
      end
    end
    data.ratings = out
  end)
end

local function pullSource()
  call({ "source", "--json" }, function(got)
    local p = type(got) == "table" and got.provider or nil
    data.source = type(p) == "string" and p ~= "" and p or "all"
    sourceReady = true
    pullLibrary()
  end)
end


local function pullQueue()
  call({ "queue", "--json" }, function(got)
    if type(got) == "table" then
      data.queue = type(got.tracks) == "table" and got.tracks or {}
      data.queueIndex = tonumber(got.index)
      -- Кэш артов ключуется по URL и привязан к прошлой очереди:
      -- без сброса он рос без предела на каждое queue_changed.
      -- pending не трогаем — там висят колбэки живых загрузок.
      artCache.done = {}
      -- Панель показывает 10 строк: качать арты всей очереди —
      -- раздувать каталог art и трафик. Украшаем только видимое
      -- окно: от текущего трека (queueIndex 0-based) и 14 следующих.
      local start = (data.queueIndex or 0) + 1
      for i = start, math.min(#data.queue, start + 14) do
        decorateArt("track", data.queue[i])
      end
    end
  end)
end

local function onEvent(line)
  local text = noctalia.string.trim(line)
  if text == "" then
    return
  end
  lastLineAt = noctalia.nowMs()
  local got = noctalia.json.decode(text)
  if type(got) ~= "table" then
    return
  end

  data.linked = true
  data.error = ""

  local kind = tostring(got.event or "")
  -- Keepalive — метка жизни, а не событие состояния: достаточно того,
  -- что она уже сдвинула lastLineAt и подтвердила связность.
  if kind == "keepalive" then
    return
  end
  if kind == "position" then
    data.position = seconds(got.position)
    if got.duration ~= nil then
      data.duration = seconds(got.duration)
    end
    -- 1 Гц: лёгкого ключа достаточно, тяжёлый (вся очередь, результаты,
    -- плейлисты) здесь не менялся — маршалить его каждый тик расточительно.
    publishNow()
    return
  elseif kind == "track_changed" then
    applyTrack(got.track)
    data.queueIndex = tonumber(got.queue_index)
  elseif kind == "rating_changed" then
    -- Одна запись из события: полный Cmd::Ratings ради одного трека
    -- не запрашиваем. "none" удаляет ключ — отсутствие оценки и
    -- "none" в панели одно и то же.
    local tid = type(got.track) == "table" and got.track or {}
    local key = tostring(tid.provider or "") .. ":" .. tostring(tid.id or "")
    if type(data.ratings) ~= "table" then
      data.ratings = {}
    end
    if got.rating == "none" or got.rating == nil then
      data.ratings[key] = nil
    else
      data.ratings[key] = tostring(got.rating)
    end
  elseif kind == "playlists_changed" then
    -- Демон шлёт только сигнал без списка: перечитываем library целиком —
    -- полный ответ дешевле инкрементальных правок на 18 плейлистов, а
    -- панель обновится через watch тяжёлого ключа после publish().
    pullLibrary()
  elseif kind == "state_changed" then
    applyState(got.state)
  elseif kind == "queue_changed" then
    data.queueLen = tonumber(got.len) or 0
    data.queueIndex = tonumber(got.index)
    pullQueue()
  elseif kind == "auth_changed" then
    local auth = got.auth
    local hint = type(auth) == "table" and (auth.hint or auth.Missing or auth.Expired) or nil
    data.error = type(hint) == "string" and hint or ("сессия " .. tostring(got.provider))
    noctalia.notifyError("Музыка", data.error)
    pullProviders()
  end
  publish()
end

local function connect()
  if streaming then
    return
  end
  retryAt = noctalia.nowMs() + RETRY_MS
  streaming = noctalia.runStream(TMUS .. " events", function(line)
    onEvent(line)
  end)
  if not streaming then
    data.linked = false
    data.error = "tmusd не отвечает"
    publish()
  end
end

local function refreshAll()
  pullStatus()
  pullProviders()
  pullSource()
  pullQueue()
  pullRatings()
  pullHome()
end

function update()
  if data.linked and noctalia.nowMs() - lastLineAt > STALE_MS then
    -- Поток умер молча (перезапуск демона, обрыв сокета): у runStream
    -- нет onClose, тишина дольше трёх keepalive — единственный след.
    -- Без этой ветки виджет замерзал на последнем треке до ручного
    -- открытия панели — панель дёргает "refresh", и только он чинил
    -- картинку. Чинимся тем же путём, что и старт без демона.
    streaming = false
    data.linked = false
    data.error = "поток событий оборвался"
    publish()
    connect()
    refreshAll()
  end
  if not data.linked and noctalia.nowMs() >= retryAt then
    streaming = false
    connect()
    refreshAll()
  end
  if sourceReady and noctalia.nowMs() - libraryAt >= LIBRARY_MS then
    pullLibrary()
  end
  if sourceReady and noctalia.nowMs() - homeAt >= HOME_MS then
    pullHome()
  end
  if artDirty then
    artDirty = false
    publish()
  end
  if homeDirty then
    -- Арты ленты дозагрузились: перевыпускаем только её ключ,
    -- тяжёлый tmus не трогаем.
    homeDirty = false
    publishHome()
  end
  -- GC каталога art (аудит 18.09): API перечисления файлов у noctalia
  -- нет (только fileExists/removeFile) — дисковую очистку написать нечем.
end

function onIpc(event, payload)
  if event == "toggle" then
    poke({ "toggle" })
  elseif event == "next" then
    poke({ "next" })
  elseif event == "prev" then
    poke({ "prev" })
  elseif event == "stop" then
    poke({ "stop" })
  elseif event == "vol-up" then
    poke({ "vol", "+5" })
  elseif event == "vol-down" then
    poke({ "vol", "-5" })
  elseif event == "vol-by" and payload then
    -- Скролл виджета шлёт одно агрегированное событие ("+15"/"-10");
    -- tmus vol понимает знак.
    poke({ "vol", tostring(payload) })
  elseif event == "vol-set" and payload then
    -- Ползунок панели шлёт абсолютное значение ("70"); tmus vol
    -- принимает и его.
    poke({ "vol", tostring(payload) })
  elseif event == "play" and payload then
    poke({ "play", payload })
  elseif event == "play-context" and payload then
    -- Панель шлёт "<start>|<prov:id>,<prov:id>,..."; CLI —
    -- `tmus play-context <ids-через-запятую> --start <n>`. Разбор по
    -- образцу "play-playlist"; ответ не нужен — демон разошлёт state.
    local index, ids = tostring(payload):match("^(%d+)|(.+)$")
    if ids and ids ~= "" then
      poke({ "play-context", ids, "--start", tostring(math.max(0, tonumber(index) or 0)) })
    end
  elseif event == "rate" and payload then
    -- Панель шлёт "like|dislike|none:<provider>:<id>"; argv CLI —
    -- `tmus rate <provider>:<id> like|dislike|none`. Ответ не нужен:
    -- демон разошлёт rating_changed, и запись обновится сама.
    local want, id = tostring(payload):match("^([%a]+):(.+)$")
    -- Защита в глубину: панель шлёт канонические like|dislike, но
    -- принимаем и snapshot-формы "liked"/"disliked" — молчаливый
    -- дроп события хуже лишней нормализации.
    if want == "liked" then want = "like" end
    if want == "disliked" then want = "dislike" end
    if (want == "like" or want == "dislike" or want == "none") and id and id ~= "" then
      poke({ "rate", id, want })
    end
  elseif event == "pl" and payload then
    -- Кастомные плейлисты: панель шлёт "new:<title>" |
    -- "newadd:<title>:<trackProv>:<trackId>" |
    -- "add:<plProv>:<plId>:<trackComposite>" |
    -- "rm:<plProv>:<plId>:<trackComposite>" | "del:<plProv>:<plId>".
    -- Идентификатор плейлиста СОСТАВНОЙ — CLI парсит первый аргумент
    -- как <provider>:<id> и голый id отвергает (живой замер 19.09).
    -- После успешного вызова перечитываем библиотеку целиком: демон
    -- шлёт только сигнал (playlists_changed), полный список дешевле
    -- инкрементальных правок на 18 плейлистов.
    local op, rest = tostring(payload):match("^([%a]+):(.*)$")
    if op == "new" then
      -- Интерактивный плейлист создаётся ЛОКАЛЬНЫМ по умолчанию:
      -- CLI `pl new <title>` без --provider.
      call({ "pl", "new", rest }, nil, function() pullLibrary() end)
    elseif op == "newadd" then
      -- Создание + немедленное добавление трека. Плейлист создаём
      -- ЛОКАЛЬНЫМ (дефолт CLI): адресат по умолчанию — всегда
      -- доступное хранилище, нативные адресата панель предлагает
      -- только при совпадении провайдера и caps.playlist_add. id
      -- нового плейлиста существует только в stdout `pl new` (голая
      -- строка, не JSON), поэтому цепочка живёт здесь: call()
      -- прогоняет stdout через json.decode и голый id наружу не
      -- отдаёт — runAsync напрямую, по образцу call(). Панель слала
      -- бы add по ИМЕНИ — гонка с дубликатами имён; в цепочке id
      -- точный.
      local title, prov, tid = rest:match("^([^:]+):([^:]+):(.+)$")
      if title and prov and tid then
        noctalia.runAsync({ TMUS, "pl", "new", title }, function(res)
          if not res or res.exitCode ~= 0 then
            data.error = res and noctalia.string.trim(res.stderr or "") ~= ""
              and noctalia.string.trim(res.stderr) or "tmus не ответил"
            publish()
            return
          end
          local plId = noctalia.string.trim(res.stdout or "")
          if plId == "" then
            return
          end
          call({ "pl", "add", "local:" .. plId, prov .. ":" .. tid }, nil, function() pullLibrary() end)
        end)
      end
    elseif op == "add" or op == "rm" then
      -- Первые два сегмента без ':' — провайдер и голый id плейлиста,
      -- жадный хвост (.+) — составной id трека "prov:id": двоеточия
      -- в нём расколу не мешают.
      local plProv, plBare, track = rest:match("^([^:]+):([^:]+):(.+)$")
      if plProv and plBare and track then
        call({ "pl", op, plProv .. ":" .. plBare, track }, nil, function() pullLibrary() end)
      end
    elseif op == "rmat" then
      -- Удаление из ЛОКАЛЬНОГО плейлиста по абсолютной позиции
      -- (PlaylistRemoveAt): позиция, а не id трека — дубликаты
      -- уходят ровно по одному. rest = "<prov>:<plid>:<0-based pos>";
      -- плейлист заведомо локальный (панель шлёт rmat только для
      -- provider == "local").
      local plProv, plBare, pos = rest:match("^([^:]+):([^:]+):(%d+)$")
      if plProv and plBare then
        call({ "pl", "rm-at", plProv .. ":" .. plBare, pos }, nil, function() pullLibrary() end)
      end
    elseif op == "del" and rest ~= "" then
      -- rest уже составной "<prov>:<id>" — CLI принимает как есть.
      call({ "pl", "del", rest }, nil, function() pullLibrary() end)
    end
  elseif event == "play-playlist" and payload then
    -- PlayPlaylist { playlist, track }: составной id плейлиста,
    -- опционально "|<prov:id>" конкретного трека. CLI —
    -- `tmus play-playlist <pl> [--track <prov:id>]`; --start больше
    -- не существует — идентичность трека переходит границу сервиса
    -- сама, а не через индекс отрисовки.
    local pl, track = tostring(payload):match("^([^|]+)|(.+)$")
    if pl and pl ~= "" then
      poke({ "play-playlist", pl, "--track", track })
    elseif tostring(payload) ~= "" then
      -- Активация заголовка плейлиста: без --track.
      poke({ "play-playlist", tostring(payload) })
    end
  elseif event == "radio" and payload then
    -- Радио по треку (`tmus radio <prov:id>`): одиночная активация
    -- строки поиска/карточки home. Контекстные жесты (плейлист,
    -- альбом) идут прежними событиями.
    local id = tostring(payload)
    if id ~= "" then
      poke({ "radio", id })
    end
  elseif event == "home-more" and payload ~= nil then
    -- Курсор продолжения приходит от панели зеркалом homeState.next;
    -- настоящий guard и слияние живут здесь (pullHomeMore).
    pullHomeMore(tostring(payload))
  elseif event == "queue-goto" and payload then
    -- Payload панели — 0-базный индекс очереди; CLI понимает
    -- `tmus queue-goto <n>` напрямую.
    poke({ "queue-goto", tostring(payload) })
  elseif event == "search" and payload then
    local rawKind, rawQuery = tostring(payload):match("^([^|]+)|(.*)$")
    if rawKind == nil then
      rawKind, rawQuery = "tracks", tostring(payload)
    end
    local KIND_TOKENS = {
      all = "all",
      track = "tracks",
      tracks = "tracks",
      artist = "artists",
      artists = "artists",
      playlist = "playlists",
      playlists = "playlists",
    }
    local kindToken = KIND_TOKENS[rawKind] or "tracks"
    call({ "search", urlDecode(rawQuery), "--kind", kindToken, "--provider", data.source, "--json" }, function(got)
      data.results = type(got) == "table" and got or {}
      for _, item in ipairs(data.results) do
        local kind = "result-playlist"
        if type(item) == "table" and (type(item.artists) == "table" or type(item.album) == "string") then
          kind = "result-track"
        end
        decorateArt(kind, item)
      end
    end, function()
      data.searchSerial = data.searchSerial + 1
    end)
  elseif event == "tracks" and payload then
    call({ "library-tracks", tostring(payload), "--json" }, function(got)
      data.tracks = type(got) == "table" and got or {}
      data.tracksId = tostring(payload)
      -- Полный список мог прийти неполным: демон режет выдачу
      -- потолком batch — хвост добираем ленивой догрузкой.
      data.hasMore = true
      for _, tr in ipairs(data.tracks) do
        decorateArt("track", tr)
      end
    end)
  elseif event == "tracks-more" and payload then
    -- Ленивая догрузка хвоста: панель стучится, когда курсор подошёл
    -- к концу списка. Повторный запрос, пока летит предыдущий,
    -- схлопываем флагом — cb/onDone его снимут.
    if tracksMoreInFlight then
      return
    end
    tracksMoreInFlight = true
    local want = tostring(payload)
    call({ "library-tracks-more", want, "--json" }, function(got)
      tracksMoreInFlight = false
      -- Пока летел запрос, открыли другой плейлист: страница чужая —
      -- дописывать её в текущий список нельзя.
      if data.tracksId ~= want then
        return
      end
      local page = type(got) == "table" and got or {}
      local pageTracks = type(page.tracks) == "table" and page.tracks or {}
      if #pageTracks > 0 then
        for _, tr in ipairs(pageTracks) do
          table.insert(data.tracks, tr)
          decorateArt("track", tr)
        end
        -- Курсор продолжения живёт только в демоне: есть next — есть хвост.
        data.hasMore = page.next ~= nil and page.next ~= ""
      else
        -- Пустая страница = дочитано: панель перестаёт стучаться.
        data.hasMore = false
      end
    end, function()
      tracksMoreInFlight = false
    end)
    -- publish() здесь не нужен: call() публикует сам в onDone (так же
    -- живёт ветка "tracks" выше).
  elseif event == "source" and payload then
    local want = tostring(payload)
    if want ~= data.source then
      poke({ "source", want })
      data.source = want
      pullLibrary()
    end
  elseif event == "repeat" and payload then
    poke({ "loop", tostring(payload) })
  elseif event == "shuffle" and payload then
    poke({ "shuffle", tostring(payload) })
  elseif event == "eq" and payload then
    -- Payload панели → флаги `tmus eq`; валидность имени/диапазона
    -- проверяет CLI и демон, сюда дублировать незачем.
    local what = tostring(payload)
    if what == "on" then
      poke({ "eq", "--on" })
    elseif what == "off" then
      poke({ "eq", "--off" })
    elseif what == "flat" then
      poke({ "eq", "--preset", "Flat" })
    elseif string.sub(what, 1, 10) == "preset-on:" then
      -- Выбор пресета включает эквалайзер (приёмка 19.09): один CLI-вызов
      -- несёт оба флага, демон применяет их атомарным Equalizer-кадром.
      poke({ "eq", "--on", "--preset", string.sub(what, 11) })
    elseif string.sub(what, 1, 5) == "band:" then
      local n, g = what:match("^band:(%d+):(.-)$")
      if n and g ~= "" then
        poke({ "eq", "--band", n .. ":" .. g })
      end
    end
  elseif event == "tui" then
    local template = noctalia.getConfig("terminal")
    if type(template) ~= "string" or template == "" then
      template = "kitty -e %s"
    end
    noctalia.runAsync((string.gsub(template, "%%s", TMUS)))
  elseif event == "restart-daemon" then
    -- Полный перезапуск демона из вида Clients: перечитывает cookies
    -- браузера (логин в SoundCloud после старта демона) и библиотеки.
    -- Сокет обрывается, watch переподключается сам; очередь и громкость
    -- демон персистит при выходе.
    noctalia.runAsync({ "systemctl", "--user", "restart", "tmusd" }, function(res)
      if res and res.exitCode ~= 0 then
        data.error = (res.stderr and noctalia.string.trim(res.stderr) or "") ~= ""
          and noctalia.string.trim(res.stderr) or "restart tmusd не удался"
        publish()
      end
    end)
  elseif event == "refresh" then
    refreshAll()
  elseif event == "reconnect" then
    streaming = false
    retryAt = 0
    data.linked = false
    connect()
    refreshAll()
  end
end

function onExit()
  data.linked = false
  publish()
end

noctalia.setUpdateInterval(2000)
publish()
connect()
refreshAll()
