--!nonstrict

local KEY = "tmus"

local TMUS = noctalia.commandExists("tmus") and "tmus"
  or noctalia.expandPath("~/.cargo/bin/tmus")

local RETRY_MS = 3000
local CALL_MS = 15000
local LIBRARY_MS = 300000

local data = {
  linked = false,
  error = "",
  status = "stopped",
  title = "",
  artist = "",
  album = "",
  provider = "",
  artUrl = "",
  position = 0,
  duration = 0,
  volume = 0,
  loop = "none",
  shuffle = false,
  offline = false,
  source = "all",
  providers = {},
  playlists = {},
  queue = {},
  queueIndex = nil,
  results = {},
  tracks = {},
  tracksId = "",
  tracksTitle = "",
  at = 0,
}

local streaming = false
local retryAt = 0
local libraryAt = 0
local sourceReady = false

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

local function publish()
  data.at = os.time()
  noctalia.state.set(KEY, data)
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
  if type(track) == "table" then
    data.duration = seconds(track.duration)
  else
    data.duration = 0
  end
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
  data.queueLen = tonumber(state.queue_len) or 0
  data.queueIndex = tonumber(state.queue_index)
  applyTrack(state.track)
  if data.duration == 0 then
    data.duration = seconds(state.duration)
  end
end

local function call(args, onOk)
  local argv = { TMUS }
  for _, a in ipairs(args) do
    table.insert(argv, a)
  end
  noctalia.runAsync(argv, function(res)
    if not res or res.exitCode ~= 0 then
      local why = res and noctalia.string.trim(res.stderr or "") or ""
      data.error = why ~= "" and why or "tmus не ответил"
      data.linked = false
      publish()
      return
    end
    data.error = ""
    data.linked = true
    if onOk then
      onOk(noctalia.json.decode(res.stdout or ""))
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
  end)
end

local function pullProviders()
  call({ "providers", "--json" }, function(got)
    local out = {}
    for _, p in ipairs(type(got) == "table" and got or {}) do
      local auth = p.auth
      local kind = type(auth) == "table" and tostring(next(auth) or "") or tostring(auth or "")
      if kind == "Ready" or kind == "Anonymous" then
        table.insert(out, { id = tostring(p.id or ""), name = tostring(p.name or p.id or "") })
      end
    end
    data.providers = out
  end)
end

local function pullLibrary()
  libraryAt = noctalia.nowMs()
  call({ "library", "--provider", data.source, "--json" }, function(got)
    data.playlists = type(got) == "table" and got or {}
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
    end
  end)
end

local function onEvent(line)
  local text = noctalia.string.trim(line)
  if text == "" then
    return
  end
  local got = noctalia.json.decode(text)
  if type(got) ~= "table" then
    return
  end

  data.linked = true
  data.error = ""

  local kind = tostring(got.event or "")
  if kind == "position" then
    data.position = seconds(got.position)
    if got.duration ~= nil then
      data.duration = seconds(got.duration)
    end
  elseif kind == "track_changed" then
    applyTrack(got.track)
    data.queueIndex = tonumber(got.queue_index)
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
end

function update()
  if not data.linked and noctalia.nowMs() >= retryAt then
    streaming = false
    connect()
    refreshAll()
  end
  if sourceReady and noctalia.nowMs() - libraryAt >= LIBRARY_MS then
    pullLibrary()
  end
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
  elseif event == "play" and payload then
    poke({ "play", payload })
  elseif event == "playlist" and payload then
    poke({ "play-playlist", payload, "--start", "0" })
  elseif event == "playlist-at" and payload then
    local index, id = tostring(payload):match("^(%d+)|(.+)$")
    poke({ "play-playlist", id or "", "--start", tostring(math.max(0, tonumber(index) or 0)) })
  elseif event == "queue-goto" and payload then
    poke({ "play", payload })
  elseif event == "search" and payload then
    call({ "search", urlDecode(payload), "--provider", data.source, "--json" }, function(got)
      data.results = type(got) == "table" and got or {}
    end)
  elseif event == "tracks" and payload then
    call({ "library-tracks", tostring(payload), "--json" }, function(got)
      data.tracks = type(got) == "table" and got or {}
      data.tracksId = tostring(payload)
    end)
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
  elseif event == "tui" then
    local template = noctalia.getConfig("terminal")
    if type(template) ~= "string" or template == "" then
      template = "kitty -e %s"
    end
    noctalia.runAsync((string.gsub(template, "%%s", TMUS)))
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
