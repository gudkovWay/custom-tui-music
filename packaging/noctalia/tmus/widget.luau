--!nonstrict

local KEY = "tmus"
local PANEL = "q/tmus:library"
local SERVICE = "q/tmus:watch"

local TITLE_WIDTH = math.max(0, tonumber(noctalia.getConfig("title_width")) or 28)
local ART = 18

local snapshot = {}

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

local function stateName(s)
  if not s.linked then
    return "disconnected"
  end
  if s.status == "playing" then
    return "playing"
  end
  if s.status == "paused" then
    return "paused"
  end
  return "stopped"
end

local function label(s)
  if not s.linked then
    return "tmusd offline"
  end
  if (s.title or "") == "" then
    return stateName(s)
  end
  local text = s.title
  if (s.artist or "") ~= "" then
    text = s.artist .. " — " .. s.title
  end
  return shorten(text, TITLE_WIDTH)
end

local function tooltip(s)
  local rows = {}
  if not s.linked then
    table.insert(rows, { key = "state", value = "disconnected" })
    table.insert(rows, { key = "error", value = (s.error or "") ~= "" and s.error or "tmusd is not running" })
    table.insert(rows, { key = "hint", value = "systemctl --user start tmusd" })
    return rows
  end
  table.insert(rows, { key = "state", value = stateName(s) })
  if (s.error or "") ~= "" then
    table.insert(rows, { key = "error", value = s.error })
  end
  if (s.title or "") ~= "" then
    table.insert(rows, { key = "track", value = s.title })
    if (s.artist or "") ~= "" then
      table.insert(rows, { key = "artist", value = s.artist })
    end
    if (s.album or "") ~= "" then
      table.insert(rows, { key = "album", value = s.album })
    end
    local dur = tonumber(s.duration) or 0
    if dur > 0 then
      table.insert(rows, { key = "time", value = clock(s.position) .. " / " .. clock(dur) })
    else
      table.insert(rows, { key = "time", value = clock(s.position) })
    end
    if (s.provider or "") ~= "" then
      table.insert(rows, { key = "source", value = s.provider .. (s.offline and " · cached" or "") })
    end
  end
  if (tonumber(s.queueLen) or 0) > 0 then
    local index = tonumber(s.queueIndex)
    local place = index ~= nil and tostring(index + 1) .. "/" .. tostring(s.queueLen)
      or tostring(s.queueLen)
    table.insert(rows, { key = "queue", value = place })
  end
  table.insert(rows, { key = "client", value = tostring(s.source or "all") })
  table.insert(rows, { key = "volume", value = string.format("%d%%", math.floor(tonumber(s.volume) or 0)) })
  local mods = {}
  if s.shuffle then
    table.insert(mods, "shuffled")
  end
  if (s.loop or "none") ~= "none" then
    table.insert(mods, "repeat: " .. tostring(s.loop))
  end
  if #mods > 0 then
    table.insert(rows, { key = "mode", value = table.concat(mods, " · ") })
  end
  return rows
end

local function glyphFor(s)
  if not s.linked then
    return "plug-off", "error"
  end
  if s.status == "paused" then
    return "player-pause", "secondary"
  end
  return "music", "on_surface_variant"
end

local function artNode(s)
  local art = tostring(s.artPath or "")
  if art ~= "" and s.status ~= "stopped" and s.linked then
    return ui.image({
      key = "art",
      path = art,
      width = ART,
      height = ART,
      radius = ART // 2,
      fit = "cover",
      opacity = s.status == "paused" and 0.6 or 1.0,
    })
  end
  local glyph, color = glyphFor(s)
  return ui.glyph({ key = "glyph", name = glyph or "music", color = color or "on_surface_variant" })
end

local function draw()
  local s = snapshot
  local text = label(s)
  local children = { artNode(s) }
  if text ~= "" then
    table.insert(children, ui.label({ key = "title", text = text, color = "on_surface" }))
  end
  barWidget.render(ui.row({ gap = 6, align = "center" }, children))
  barWidget.setTooltip(tooltip(s))
end

local function refresh(state)
  snapshot = state or {}
  draw()
end

function onClick()
  noctalia.togglePanel(PANEL)
end

function onMiddleClick()
  poke("next")
end

function onRightClick()
  poke("toggle")
end

function onScroll(axis, steps)
  local count = math.abs(tonumber(steps) or 0)
  if count == 0 then
    return
  end
  local event = (tonumber(steps) or 0) < 0 and "vol-up" or "vol-down"
  for _ = 1, count do
    poke(event)
  end
end

noctalia.state.watch(KEY, refresh)
refresh(noctalia.state.get(KEY))
