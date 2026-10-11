-- Blossom weave: общий материал для панелей и виджетов Noctalia.
--
-- Один модуль на все потребители. Потребитель копирует этот файл и соседний
-- каталог skin-assets/ (собран tools/build-blossom-weave.py) в корень своего
-- плагина и подключает модуль (нужен API плагина 22 — require):
--
--   local skin = require("./desktop_skin.luau")
--   panel.render(skin.frame(body, "studio-frame"))
--
-- frame() — рамка в потоке разметки: восемь растянутых картинок материала
-- вокруг содержимого, EDGE логических px с каждой стороны (2*EDGE на ось).
-- Внутри рамки нет ни оверлея поверх содержимого, ни кликов, ни захвата
-- фокуса — только декоративные картинки и контейнеры. Пути картинок
-- относительны каталога плагина.

local M = {}

--: Толщина рейки и размер угла, логические px; frame() резервирует 2*EDGE на ось.
M.EDGE = 12
--: Шрифт текста, подписей и метрик.
M.FONT = "Noto Sans"
--: Nerd-Font — только для глифов и числовых меток.
M.MONO = "Agave Nerd Font"

local ASSETS = "skin-assets/"

-- Угол: фиксированный квадрат EDGE x EDGE.
local function corner(key, name)
  return ui.image({
    key = key,
    path = ASSETS .. name .. ".png",
    width = M.EDGE,
    height = M.EDGE,
    fit = "stretch",
  })
end

-- Горизонтальная рейка растёт по ширине; вертикальная растягивается по высоте строкой.
local function rail(key, name, horizontal)
  local props = {
    key = key,
    path = ASSETS .. name .. ".png",
    fit = "stretch",
  }
  if horizontal then
    props.height = M.EDGE
    props.flexGrow = 1
  else
    props.width = M.EDGE
  end
  return ui.image(props)
end

--- Оборачивает содержимое в общую рамку blossom-weave.
-- Середина (рельсы + содержимое) — единственный гибкий узел дерева.
-- @param content узел содержимого панели
-- @param key стабильный префикс ключей; по умолчанию "blossom"
function M.frame(content, key)
  if type(key) ~= "string" or key == "" then
    key = "blossom"
  end
  return ui.column({ key = key, gap = 0, padding = 0, flexGrow = 1 }, {
    ui.row({ key = key .. "-top", gap = 0, height = M.EDGE }, {
      corner(key .. "-corner-tl", "corner-tl"),
      rail(key .. "-rail-top", "rail-top", true),
      corner(key .. "-corner-tr", "corner-tr"),
    }),
    ui.row({ key = key .. "-middle", gap = 0, flexGrow = 1 }, {
      rail(key .. "-rail-left", "rail-left", false),
      ui.column({ key = key .. "-content", gap = 0, flexGrow = 1 }, { content }),
      rail(key .. "-rail-right", "rail-right", false),
    }),
    ui.row({ key = key .. "-bottom", gap = 0, height = M.EDGE }, {
      corner(key .. "-corner-bl", "corner-bl"),
      rail(key .. "-rail-bottom", "rail-bottom", true),
      corner(key .. "-corner-br", "corner-br"),
    }),
  })
end

return M
