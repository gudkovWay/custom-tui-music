--!nonstrict

local KEY = "tmus"
local PANEL = "q/tmus:library"
local SERVICE = "q/tmus:watch"

local TITLE_WIDTH = math.max(0, tonumber(noctalia.getConfig("title_width")) or 28)
local ART = 22
local PROGRESS_W = 96

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

local function label(s)
  if not s.linked then
    return "нет демона"
  end
  if (s.title or "") == "" then
    return ""
  end
  local text = s.title
  if (s.artist or "") ~= "" then
    text = s.artist .. " — " .. s.title
  end
  return shorten(text, TITLE_WIDTH)
end

local function glyphFor(s)
  if not s.linked then
    return "plug-off", "error"
  end
  if (s.error or "") ~= "" then
    return "music-off", "error"
  end
  if s.status == "playing" then
    return nil
  end
  if s.status == "paused" then
    return "player-pause", "secondary"
  end
  return "music", "on_surface_variant"
end

local function tooltip(s)
  local rows = {}
  if not s.linked then
    table.insert(rows, { key = "демон", value = (s.error or "") ~= "" and s.error or "не запущен" })
    table.insert(rows, { key = "запустить", value = "systemctl --user start tmusd" })
    return rows
  end
  if (s.error or "") ~= "" then
    table.insert(rows, { key = "ошибка", value = s.error })
  end
  if (s.title or "") ~= "" then
    table.insert(rows, { key = "трек", value = s.title })
    if (s.artist or "") ~= "" then
      table.insert(rows, { key = "артист", value = s.artist })
    end
    if (s.album or "") ~= "" then
      table.insert(rows, { key = "альбом", value = s.album })
    end
    local dur = tonumber(s.duration) or 0
    if dur > 0 then
      table.insert(rows, { key = "время", value = clock(s.position) .. " / " .. clock(dur) })
    else
      table.insert(rows, { key = "время", value = clock(s.position) })
    end
  else
    table.insert(rows, { key = "трек", value = "ничего не играет" })
  end
  if (s.provider or "") ~= "" then
    table.insert(rows, { key = "источник", value = s.provider .. (s.offline and " · из кэша" or "") })
  end
  if (tonumber(s.queueLen) or 0) > 0 then
    local index = tonumber(s.queueIndex)
    local place = index ~= nil and tostring(index + 1) .. "/" .. tostring(s.queueLen)
      or tostring(s.queueLen)
    table.insert(rows, { key = "очередь", value = place })
  end
  table.insert(rows, { key = "клиент", value = tostring(s.source or "all") })
  table.insert(rows, { key = "громкость", value = string.format("%d%%", math.floor(tonumber(s.volume) or 0)) })
  local mods = {}
  if s.shuffle then
    table.insert(mods, "перемешано")
  end
  if (s.loop or "none") ~= "none" then
    table.insert(mods, "повтор: " .. tostring(s.loop))
  end
  if #mods > 0 then
    table.insert(rows, { key = "режим", value = table.concat(mods, " · ") })
  end
  return rows
end

local function artNode(s)
  local art = tostring(s.artUrl or "")
  if art ~= "" and s.status ~= "stopped" then
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
  return ui.glyph({ key = "glyph", name = glyph or "player-play", color = color or "primary" })
end

local function progressNode(s)
  local dur = tonumber(s.duration) or 0
  if dur <= 0 or s.status == "stopped" then
    return nil
  end
  local pos = tonumber(s.position) or 0
  return ui.progress({
    key = "progress",
    progress = math.max(0, math.min(1, pos / dur)),
    width = PROGRESS_W,
    height = 2,
    radius = 1,
  })
end

local function draw()
  local s = snapshot
  local children = { artNode(s) }

  local text = label(s)
  if text ~= "" then
    table.insert(children, ui.label({ key = "title", text = text, color = "on_surface" }))
  end

  local progress = progressNode(s)
  if progress then
    table.insert(children, progress)
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
  poke("toggle")
end

function onRightClick()
  poke("next")
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
