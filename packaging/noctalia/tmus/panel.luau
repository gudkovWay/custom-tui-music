--!nonstrict

local KEY = "tmus"
local SERVICE = "q/tmus:watch"

local VIEWS = {
  { id = "library", title = "Library" },
  { id = "playlist", title = "Playlist" },
  { id = "queue", title = "Queue" },
  { id = "search", title = "Search" },
  { id = "clients", title = "Clients" },
}
local VIEW_IDS = {}
for i, v in ipairs(VIEWS) do
  VIEW_IDS[v.id] = i
end

local LAYOUT = {
  G = "g", H = "h", J = "j", K = "k", L = "l",
  N = "n", O = "o", P = "p", R = "r", S = "s",
  Cyrillic_pe = "g", Cyrillic_er = "h", Cyrillic_o = "j", Cyrillic_el = "k",
  Cyrillic_de = "l", Cyrillic_te = "n", Cyrillic_shcha = "o", Cyrillic_ze = "p",
  Cyrillic_ka = "r", Cyrillic_yeru = "s",
  Cyrillic_PE = "g", Cyrillic_ER = "h", Cyrillic_O = "j", Cyrillic_EL = "k",
  Cyrillic_DE = "l", Cyrillic_TE = "n", Cyrillic_SHCHA = "o", Cyrillic_ZE = "p",
  Cyrillic_KA = "r", Cyrillic_YERU = "s",
}

local REPEAT_CYCLE = { none = "track", track = "queue", queue = "none" }
local REPEAT_LABEL = { none = "repeat off", track = "repeat track", queue = "repeat queue" }

local ART = 72

local snapshot = {}
local view = "library"
local cursor = 1
local playlistTitle = ""
local playlistId = ""
local query = ""
local searching = false

local function poke(event, payload)
  local cmd = "noctalia msg plugin " .. SERVICE .. " all " .. event
  if payload then
    cmd = cmd .. " " .. payload
  end
  noctalia.runAsync(cmd)
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

local function trackTitle(t)
  local artists = type(t.artists) == "table" and table.concat(t.artists, ", ") or ""
  return artists ~= "" and (artists .. " — " .. tostring(t.title or "")) or tostring(t.title or "")
end

local function playlistCount(p)
  local count = tonumber(p.track_count)
  if count then
    return tostring(count) .. " треков"
  end
  return tostring(p.subtitle or "")
end

local function resultRow(r)
  local kind = tostring(r.kind or "")
  local artists = type(r.artists) == "table" and table.concat(r.artists, ", ") or ""
  local t = {
    title = tostring(r.title or r.name or ""),
    subtitle = kind == "track" and artists or (kind == "playlist" and "плейлист" or "артист"),
  }
  if kind == "track" then
    t.event = "play"
    t.payload = idOf(r)
  elseif kind == "playlist" then
    t.event = nil
    t.open = idOf(r)
    t.openTitle = t.title
  end
  return t
end

local function rows()
  local s = snapshot
  local out = {}

  if view == "library" then
    for _, p in ipairs(s.playlists or {}) do
      table.insert(out, {
        title = tostring(p.title or ""),
        subtitle = playlistCount(p),
        badge = tostring(p.id and p.id.provider or ""),
        open = idOf(p),
        openTitle = tostring(p.title or ""),
      })
    end
  elseif view == "playlist" then
    if tostring(s.tracksId or "") ~= playlistId then
      return out
    end
    for i, t in ipairs(s.tracks or {}) do
      table.insert(out, {
        title = trackTitle(t),
        subtitle = tostring(t.album or ""),
        badge = tostring(t.id and t.id.provider or ""),
        event = "playlist-at",
        payload = tostring(i - 1) .. "|" .. playlistId,
      })
    end
  elseif view == "queue" then
    local index = tonumber(s.queueIndex)
    for i, t in ipairs(s.queue or {}) do
      table.insert(out, {
        title = (index ~= nil and i == index + 1 and "▶ " or "") .. tostring(t.title or ""),
        subtitle = type(t.artists) == "table" and table.concat(t.artists, ", ") or "",
        badge = tostring(t.id and t.id.provider or ""),
        event = "play",
        payload = idOf(t),
      })
    end
  elseif view == "search" then
    for _, r in ipairs(s.results or {}) do
      local row = resultRow(r)
      row.badge = type(r.id) == "table" and tostring(r.id.provider or "")
        or tostring(r.provider or "")
      table.insert(out, row)
    end
  elseif view == "clients" then
    table.insert(out, {
      title = "All",
      subtitle = "все подключённые клиенты",
      badge = "",
      event = "source",
      payload = "all",
      active = tostring(s.source or "all") == "all",
    })
    for _, p in ipairs(s.providers or {}) do
      table.insert(out, {
        title = p.name,
        subtitle = p.id,
        badge = p.id,
        event = "source",
        payload = p.id,
        active = tostring(s.source or "all") == p.id,
      })
    end
  end

  return out
end

local function openPlaylist(row)
  playlistTitle = row.openTitle or row.title
  playlistId = row.open
  view = "playlist"
  cursor = 1
  poke("tracks", row.open)
end

local function activate(row)
  if row.open then
    openPlaylist(row)
  elseif row.event == "playlist-at" then
    poke(row.event, row.payload)
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
end

local function setView(id)
  view = id
  cursor = 1
  searching = false
end

local function cycleView(step)
  local i = VIEW_IDS[view] or 1
  i = i + step
  if i < 1 then
    i = #VIEWS
  elseif i > #VIEWS then
    i = 1
  end
  setView(VIEWS[i].id)
end

local function nowPlaying(s)
  if not s.linked then
    return {
      ui.label({ text = "демон не отвечает · systemctl --user start tmusd", color = "error" }),
    }
  end
  if (s.title or "") == "" then
    return { ui.label({ text = "ничего не играет", color = "on_surface_variant" }) }
  end

  local dur = tonumber(s.duration) or 0
  local pos = tonumber(s.position) or 0
  local time = dur > 0 and (clock(pos) .. " / " .. clock(dur)) or clock(pos)
  local marks = {}
  if s.offline then
    table.insert(marks, "кэш")
  end
  if s.shuffle then
    table.insert(marks, "перемешано")
  end
  if (s.loop or "none") ~= "none" then
    table.insert(marks, REPEAT_LABEL[tostring(s.loop)] or tostring(s.loop))
  end

  local art = tostring(s.artUrl or "")
  local artNode
  if art ~= "" then
    artNode = ui.image({
      key = "np-art",
      path = art,
      width = ART,
      height = ART,
      radius = 8,
      fit = "cover",
      opacity = s.status == "paused" and 0.6 or 1.0,
    })
  else
    artNode = ui.glyph({
      key = "np-glyph",
      name = s.status == "playing" and "player-play" or "music",
      color = "primary",
    })
  end

  local children = {
    ui.row({ key = "np-top", gap = 14, align = "center" }, {
      artNode,
      ui.column({ key = "np-text", gap = 2, flexGrow = 1 }, {
        ui.label({
          key = "np-title",
          text = shorten(tostring(s.title or ""), 48),
          color = "primary",
          fontSize = 18,
        }),
        ui.label({
          key = "np-artist",
          text = shorten(tostring(s.artist or ""), 48),
          color = "on_surface_variant",
        }),
        ui.label({
          key = "np-time",
          text = time .. (#marks > 0 and ("  ·  " .. table.concat(marks, " · ")) or ""),
          color = "on_surface_variant",
        }),
      }),
    }),
  }
  if dur > 0 then
    table.insert(children, ui.progress({
      key = "np-progress",
      progress = math.max(0, math.min(1, pos / dur)),
      flexGrow = 1,
      height = 4,
      radius = 2,
    }))
  end
  table.insert(children, ui.row({ key = "np-controls", gap = 12, align = "center" }, {
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
      glyph = "shuffle",
      controlSize = "sm",
      variant = s.shuffle and "primary" or "ghost",
      onClick = function()
        poke("shuffle", "toggle")
      end,
    }),
    ui.button({
      key = "repeat",
      glyph = (s.loop or "none") == "none" and "repeat" or "repeat-once",
      controlSize = "sm",
      variant = (s.loop or "none") ~= "none" and "primary" or "ghost",
      onClick = function()
        poke("repeat", REPEAT_CYCLE[tostring(s.loop or "none")] or "none")
      end,
    }),
  }))
  return children
end

local function emptyText(items)
  if #items > 0 then
    return nil
  end
  local s = snapshot
  if not s.linked then
    return ""
  end
  if view == "library" then
    return "плейлистов нет — проверьте аккаунт"
  elseif view == "playlist" then
    return "треков нет"
  elseif view == "queue" then
    return "очередь пуста"
  elseif view == "clients" then
    return "нет подключённых клиентов"
  elseif searching then
    return "ищу…"
  else
    return "введите запрос: /"
  end
end

local function listBody(items)
  local empty = emptyText(items)
  if empty then
    return { ui.label({ key = "empty", text = empty, color = "on_surface_variant" }) }
  end

  local children = {}
  for i, row in ipairs(items) do
    local active = i == cursor
    local marks = {}
    table.insert(marks, ui.label({
      key = "badge" .. tostring(i),
      text = row.badge ~= "" and row.badge or "·",
      color = "secondary",
      width = 90,
    }))
    table.insert(marks, ui.label({
      key = "title" .. tostring(i),
      text = row.title,
      color = active and "primary" or "on_surface",
      flexGrow = 1,
    }))
    table.insert(marks, ui.label({
      key = "sub" .. tostring(i),
      text = row.subtitle or "",
      color = "on_surface_variant",
    }))
    table.insert(children, ui.row({
      key = "row" .. tostring(i),
      gap = 8,
      paddingH = 8,
      paddingV = 4,
      radius = 4,
      align = "center",
      fill = (active or row.active) and "surface_variant" or "surface_variant/0",
      onClick = function()
        cursor = i
        if row.open then
          openPlaylist(row)
        else
          activate(row)
        end
        draw()
      end
    }, marks))
  end
  return children
end

local function tabBar()
  local children = {}
  for i, t in ipairs(VIEWS) do
    table.insert(children, ui.label({
      key = "tab" .. tostring(i),
      text = t.title,
      color = view == t.id and "primary" or "on_surface_variant",
      onClick = function()
        setView(t.id)
        draw()
      end,
    }))
  end
  return ui.row({ key = "tabs", gap = 16, paddingV = 4 }, children)
end

local function helpLine()
  return ui.label({
    key = "help",
    text = "j/k список · h/l плейлист/назад · Space играть/пауза · Return открыть · "
      .. "r повтор · s перемешать · / поиск · n/p трек · o терминал · g края",
    color = "on_surface_variant",
  })
end

function draw()
  local s = snapshot
  local items = rows()
  if cursor > #items then
    cursor = math.max(1, #items)
  end

  local children = {
    ui.column({ key = "np", gap = 6 }, nowPlaying(s)),
    tabBar(),
  }

  if view == "search" then
    table.insert(children, ui.input({
      key = "search",
      value = query,
      placeholder = "что искать",
      focus = true,
      submitOnEnter = true,
      onSubmit = function(text)
        submitSearch(text)
      end,
    }))
  elseif view == "playlist" and playlistTitle ~= "" then
    table.insert(children, ui.label({
      key = "playlist-title",
      text = playlistTitle,
      color = "on_surface",
    }))
  end

  table.insert(children, ui.scroll({
    flexGrow = 1,
  }, { ui.column({ gap = 2 }, listBody(items)) }))

  table.insert(children, helpLine())

  panel.render(ui.column({ gap = 10, padding = 14 }, children))
end

local function handleViewKeys(key)
  if key == "l" then
    if view == "library" then
      local row = rows()[cursor]
      if row and row.open then
        openPlaylist(row)
      end
    elseif view ~= "playlist" then
      setView("library")
    end
  elseif key == "h" then
    if view == "playlist" then
      setView("library")
    elseif view == "library" then
      setView("clients")
    elseif view == "clients" then
      setView("search")
    elseif view == "search" then
      setView("queue")
    else
      setView("library")
    end
  end
end

function onKey(chord, pressed)
  if not pressed then
    return
  end

  local key = LAYOUT[chord] or chord

  if chord == "Escape" then
    panel.close()
    return
  elseif chord == "Return" then
    local row = rows()[cursor]
    if row then
      activate(row)
    end
  elseif chord == "Space" then
    if view == "playlist" then
      local row = rows()[cursor]
      if row then
        activate(row)
      end
    else
      poke("toggle")
    end
  elseif key == "n" then
    poke("next")
  elseif key == "p" then
    poke("prev")
  elseif key == "j" or chord == "Down" then
    move(1)
  elseif key == "k" or chord == "Up" then
    move(-1)
  elseif key == "h" or key == "l" or chord == "Left" or chord == "Right" then
    if chord == "Left" then
      key = "h"
    elseif chord == "Right" then
      key = "l"
    end
    handleViewKeys(key)
  elseif chord == "1" or chord == "2" or chord == "3" or chord == "4" or chord == "5" then
    setView(VIEWS[tonumber(chord)].id)
  elseif key == "g" then
    local count = #rows()
    cursor = cursor == 1 and count or 1
  elseif key == "r" then
    poke("repeat", REPEAT_CYCLE[tostring(snapshot.loop or "none")] or "none")
  elseif key == "s" then
    poke("shuffle", "toggle")
  elseif key == "o" then
    poke("tui")
    panel.close()
    return
  elseif key == "slash" or chord == "slash" then
    setView("search")
  end

  draw()
end

local function refresh(state)
  snapshot = state or {}
  searching = false
  draw()
end

function onOpen()
  poke("refresh")
  refresh(noctalia.state.get(KEY))
end

function submitSearch(text)
  query = noctalia.string.trim(text or "")
  if query == "" then
    return
  end
  searching = true
  poke("search", noctalia.string.urlEncode(query))
  draw()
end

noctalia.state.watch(KEY, refresh)
refresh(noctalia.state.get(KEY))
