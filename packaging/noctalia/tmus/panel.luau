--!nonstrict

local KEY = "tmus"
local SERVICE = "q/tmus:watch"

local COLORS = {
  bg = "#15110F",
  surface = "#201A17",
  selected = "#2B211D",
  accent = "#FFA987",
  text = "#F2E8E2",
  muted = "#A99B94",
  border = "#463832",
}
local FONT = "Agave Nerd Font"
local RAIL = 64
local GAP = 12
local PAD = 16

local VIEWS = {
  { id = "home", title = "Home", glyph = "home" },
  { id = "search", title = "Search", glyph = "search" },
  { id = "library", title = "Library", glyph = "library" },
  { id = "queue", title = "Queue", glyph = "playlist" },
  { id = "clients", title = "Clients", glyph = "server" },
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
local REPEAT_LABEL = { none = "Repeat off", track = "Repeat track", queue = "Repeat queue" }
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
local selected = nil
local playlistId = ""
local playlistTitle = ""
local playlistArtPath = ""
local query = ""
local searching = false
local searchKind = "all"
local lastSearchSerial = nil
local doSearch -- defined below; used by chips and submitSearch

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
  table.insert(rows, ui.row({ key = "ct-controls", gap = 8, align = "center" }, {
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
      glyph = (s.loop or "none") == "none" and "repeat" or "repeat-once",
      controlSize = "sm",
      variant = (s.loop or "none") ~= "none" and "primary" or "ghost",
      onClick = function()
        poke("repeat", REPEAT_CYCLE[tostring(s.loop or "none")] or "none")
      end,
    }),
  }))
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
  local children = {
    ui.box({ key = "mark" .. i, width = 28 }, {
      lbl(o.index or tostring(i), { key = "idx" .. i, color = current and COLORS.accent or COLORS.muted }),
    }),
    artNode(o.art, 32),
    lbl(shorten(o.title, 52), {
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
    fill = (active or current) and COLORS.selected or COLORS.bg,
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
    title = tostring(r.title or r.name or ""),
    subtitle = artists ~= "" and artists or tostring(r.album or ""),
    art = tostring(r.art_path or ""),
  }
  if kind == "track" then
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
      table.insert(out, row)
      if #out >= 200 then
        break
      end
    end
  end
  return out
end

local function rows()
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
        table.insert(out, {
          kind = "track",
          title = trackTitle(t),
          subtitle = tostring(t.album or ""),
          art = tostring(t.art_path or ""),
          event = "playlist-at",
          payload = tostring(i - 1) .. "|" .. playlistId,
          time = durationLabel(t.duration),
        })
      end
    end
  elseif view == "queue" then
    local index = tonumber(s.queueIndex)
    for i, t in ipairs(s.queue or {}) do
      if i > 200 then
        break
      end
      table.insert(out, {
        kind = "track",
        title = tostring(t.title or ""),
        subtitle = type(t.artists) == "table" and table.concat(t.artists, ", ") or "",
        art = tostring(t.art_path or ""),
        event = "play",
        payload = idOf(t),
        time = durationLabel(t.duration),
        current = index ~= nil and i == index + 1 or false,
      })
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
  end

  return out
end

local function openPlaylist(row)
  playlistTitle = row.openTitle or row.title
  playlistId = row.open
  playlistArtPath = row.openArt or ""
  view = "playlist"
  cursor = 1
  poke("tracks", row.open)
end

local function activate(row)
  if row == nil then
    return
  end
  selected = row
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
  selected = items[cursor]
end

local function setView(id)
  view = id
  cursor = 1
  selected = nil
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

local function railNode()
  local items = {}
  for i, v in ipairs(VIEWS) do
    local active = view == v.id or (view == "playlist" and v.id == "library")
    table.insert(items, ui.column({
      key = "rail-" .. v.id,
      gap = 2,
      align = "center",
      paddingV = 8,
      radius = 6,
      fill = active and COLORS.selected or COLORS.bg,
      onClick = function()
        setView(v.id)
        draw()
      end,
    }, {
      ui.glyph({ key = "rail-glyph-" .. v.id, name = v.glyph, color = active and COLORS.accent or COLORS.muted }),
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

local function listChildren(items)
  local children = {}
  local lastKind = nil
  for i, row in ipairs(items) do
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
  local children = {
    ui.row({ key = "np-hero", gap = 20, align = "center" }, {
      artNode(s.artPath, 200),
      ui.column({ key = "np-text", gap = 8, flexGrow = 1 }, {
        lbl("Now Playing", { key = "np-over", size = 12, color = COLORS.muted }),
        lbl(shorten(tostring(s.title or ""), 34), { key = "np-title", size = 26, color = COLORS.text }),
        lbl(shorten(tostring(s.artist or ""), 40), { key = "np-artist", size = 16, color = COLORS.accent }),
        lbl((tostring(s.album or "") ~= "" and shorten(s.album, 40) .. "  ·  " or "")
          .. clock(pos) .. (dur > 0 and (" / " .. clock(dur)) or "")
          .. (#marks > 0 and ("  ·  " .. table.concat(marks, " · ")) or ""), {
          key = "np-meta",
          color = COLORS.muted,
        }),
      }),
    }),
  }
  local controls = ui.row({ key = "np-controls", gap = 10, align = "center" }, {
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
      glyph = (s.loop or "none") == "none" and "repeat" or "repeat-once",
      controlSize = "md",
      variant = (s.loop or "none") ~= "none" and "primary" or "ghost",
      onClick = function()
        poke("repeat", REPEAT_CYCLE[tostring(s.loop or "none")] or "none")
      end,
    }),
  })
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

local function playlistCards(s)
  local cards = {}
  local items = rows()
  for i, row in ipairs(items) do
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
    contextTitle("Search"),
    ui.input({
      key = "home-search",
      value = query,
      placeholder = "Search",
      controlSize = "sm",
      submitOnEnter = true,
      onSubmit = function(text)
        submitSearch(text)
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

function draw()
  local s = snapshot
  local items = rows()
  if cursor > #items then
    cursor = math.max(1, #items)
  end

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
      table.insert(body, ui.scroll({ key = "results", flexGrow = 1 }, {
        ui.column({ key = "results-list", gap = 6 }, listChildren(items)),
      }))
    end
    workspace = ui.column({ key = "ws", gap = GAP, flexGrow = 63 }, {
      ui.input({
        key = "search",
        value = query,
        placeholder = "Search tracks, artists, collections",
        focus = true,
        submitOnEnter = true,
        onSubmit = function(text)
          submitSearch(text)
        end,
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
      table.insert(body, ui.scroll({ key = "library-scroll", flexGrow = 1 }, {
        ui.column({ key = "library-grid", gap = GAP }, playlistCards(s)),
      }))
    end
    workspace = ui.column({ key = "ws", gap = GAP, flexGrow = 7 }, {
      lbl("Library", { key = "ws-title", size = 18, color = COLORS.text }),
      ui.column({ key = "ws-body", gap = GAP, flexGrow = 1 }, body),
    })
    local current = currentTrack(s)
    local ctxChildren = { contextTitle("Current") }
    if current then
      table.insert(ctxChildren, current)
    else
      table.insert(ctxChildren, lbl("Nothing playing", { key = "lib-none", color = COLORS.muted }))
    end
    context = ui.column({ key = "library-context", gap = GAP, flexGrow = 3 }, ctxChildren)
  elseif view == "playlist" then
    local state = stateNode(s, items)
    local body = {}
    if state then
      table.insert(body, state)
    else
      table.insert(body, ui.scroll({ key = "playlist-scroll", flexGrow = 1 }, {
        ui.column({ key = "playlist-list", gap = 4 }, listChildren(items)),
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
      table.insert(body, ui.scroll({ key = "queue-scroll", flexGrow = 1 }, {
        ui.column({ key = "queue-list", gap = 4 }, listChildren(items)),
      }))
    end
    workspace = ui.column({ key = "ws", gap = GAP, flexGrow = 7 }, {
      lbl("Queue", { key = "ws-title", size = 18, color = COLORS.text }),
      ui.column({ key = "ws-body", gap = GAP, flexGrow = 1 }, body),
    })
    context = queueContext(s)
  else
    local state = stateNode(s, items)
    local body = {}
    if state then
      table.insert(body, state)
    else
      table.insert(body, ui.scroll({ key = "clients-scroll", flexGrow = 1 }, {
        ui.column({ key = "clients-list", gap = 6 }, listChildren(items)),
      }))
    end
    workspace = ui.column({ key = "ws", gap = GAP, flexGrow = 7 }, {
      lbl("Clients", { key = "ws-title", size = 18, color = COLORS.text }),
      ui.column({ key = "ws-body", gap = GAP, flexGrow = 1 }, body),
    })
    context = clientsContext(s)
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
    "1–5 views · j/k move · Return open · Space play/pause · h/l back/deep · / search · "
      .. "n/p track · r repeat · s shuffle · o terminal · g ends",
    { key = "footer", size = 11, color = COLORS.muted }
  )

  panel.render(ui.column({
    key = "root",
    gap = GAP,
    padding = PAD,
    fill = COLORS.bg,
  }, {
    ui.row({ key = "body", gap = GAP, flexGrow = 1 }, { railNode(), main }),
    footer,
  }))
end

local function handleViewKeys(key)
  if key == "l" then
    if view == "library" then
      local row = rows()[cursor]
      if row and row.open then
        openPlaylist(row)
      end
    elseif view ~= "playlist" then
      cycleView(1)
    end
  elseif key == "h" then
    if view == "playlist" then
      setView("library")
    else
      cycleView(-1)
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
    activate(rows()[cursor])
  elseif chord == "Space" then
    if view == "playlist" then
      activate(rows()[cursor])
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
  if lastSearchSerial == nil or snapshot.searchSerial ~= lastSearchSerial then
    searching = false
  end
  draw()
end

function onOpen()
  poke("refresh")
  refresh(noctalia.state.get(KEY))
end

function doSearch()
  lastSearchSerial = snapshot.searchSerial
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
refresh(noctalia.state.get(KEY))
