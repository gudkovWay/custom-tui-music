-- Общий материал скинов Noctalia: канонический модуль для плагинов.
--
-- Один модуль на все потребители. Потребитель копирует этот файл и соседний
-- каталог skin-assets/ в корень своего плагина и подключает модуль
-- (нужен API плагина 32 — plugin_api = 32: getSetting (26) и getColor (31)).
--
-- Режим определяется эффективным состоянием q/skin-runtime:
--   skin.stagecraft() — true только когда плагин q/skin-runtime включён
--   в [plugins].enabled, его setting family == "stagecraft" И опубликован
--   валидный applied_variant в plugin_settings["q/skin-runtime"] (variant_id
--   применённого рецепта, 32 строчных hex; дефолт "" — неактивно). Семейное
--   значение по умолчанию в манифесте — тоже "stagecraft", поэтому после
--   очистки скина family возвращает то же слово: сам по себе он активностью
--   НЕ считается.
--
-- frame() условный:
--   неактивно — прежняя рамка blossom-weave: восемь растянутых картинок,
--   EDGE логических px с каждой стороны (2*EDGE на ось), без оверлеев,
--   кликов и захвата фокуса; пути картинок — относительно каталога плагина.
--   активно — одна нативная колонка material="stagecraft" (структурный
--   рельефный периметр рисует шейдер одного rect-draw), shape="chamfer":
--   заливка role surface, цвет рамки role outline — role primary только
--   когда узел реально держит фокус (frame(body, key, { focused = true }));
--   рамка при материал-режиме — цвет того же единого каркаса, а не второй
--   тонкий контур.
--
-- Цвета: семантические роли хоста как строки ("surface", "surface_variant",
-- "on_surface", "on_surface_variant", "primary", "outline"). Реконсилютор
-- резолвит роль в актуальный цвет базовой палитры на каждом кадре — никаких
-- рецептов, замороженных значений или одноразовых снимков getColor: обои
-- меняют палитру, и цвета следуют за ней. Всё перечитывается на каждом
-- вызове: afterreload/settings перестраивают поверхности, кэш протух бы.
--
-- Никаких ui.box с детьми: контейнеры — только ui.column/ui.row.

local M = {}

--: Толщина рейки blossom-рамки и размер угла, логические px.
M.EDGE = 12
--: Шрифт текста, подписей и метрик.
M.FONT = "Noto Sans"
--: Nerd-Font — только для глифов и числовых меток.
M.MONO = "Agave Nerd Font"

-- Опциональные поля рецепта читаем НЕ отдельными getSetting-путями:
-- отсутствующий путь — warning на каждый вызов, а elided-дефолты и так
-- дают nil. Родитель plugin_settings вставляется в снапшот безусловно
-- (config_export), pushTomlNode отдаёт таблицы, поэтому один чтение
-- таблицы + обычная индексация Lua.
local VARIANT_HEX
-- нативный SkinService публикует variant_id
-- применённого рецепта (32 строчных hex) в той же атомарной проекции, что и
-- family; манифестный дефолт — "" (пусто) при неактивном/очищенном
-- скине, а при невалидной/неактивной нативной проекции ключ маскируется из
-- снапшота. Семейное значение по умолчанию активностью не считается.
local VARIANT_HEX = "^%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x%x$"

--- Свежий срез настроек q/skin-runtime из одного чтения снапшота; nil,
--- если снапшота нет или перекрытия не публиковались.
local function skinSettings()
  local ok, pluginSettings = pcall(noctalia.getSetting, "plugin_settings")
  if ok and type(pluginSettings) == "table" then
    local s = pluginSettings["q/skin-runtime"]
    if type(s) == "table" then
      return s
    end
  end
  return nil
end

local function appliedVariant(s)
  return type(s) == "table"
    and type(s.applied_variant) == "string"
    and s.applied_variant:match(VARIANT_HEX) ~= nil
end

--- Строгий hex рецепта "#rrggbb" (нативный isColorHex нормализует
--- регистр и требует ровно 7 символов): только такой цвет попадает в
--- токены Kumihimo. Иначе — рендер-фолбэк; АКТИВНОСТЬ фолбэк не
--- включает (её решает applied_variant + family).
local function hexColor(value, fallback)
  if type(value) == "string" and value:match("^#%x%x%x%x%x%x$") ~= nil then
    return value
  end
  return fallback
end

--- Активна ли семья Kumihimo: та же приёмка q/skin-runtime (плагин
--- включён в [plugins].enabled И опубликован валидный applied_variant),
--- но семья строго "kumihimo". Манифестный дефолт семьи — "stagecraft",
--- поэтому после очистки скина ключ читается как nil/"stagecraft" и
--- Kumihimo неактивен: дефолт активностью не считается.
function M.kumihimo()
  if noctalia == nil or noctalia.getSetting == nil then
    return false
  end
  local ok, enabled = pcall(noctalia.getSetting, "plugins.enabled")
  if not ok or type(enabled) ~= "table" then
    return false
  end
  local present = false
  for _, id in ipairs(enabled) do
    if id == "q/skin-runtime" then
      present = true
      break
    end
  end
  if not present then
    return false
  end
  local s = skinSettings()
  if type(s) ~= "table" or s.family ~= "kumihimo" then
    return false
  end
  return appliedVariant(s)
end

--- Токены активного материала Kumihimo. Шесть цветов — из текущего
--- снимка настроек применённого рецепта q/skin-runtime (surface,
--- surface_variant, text, muted, accent, outline), НЕ роли базовой
--- палитры: обои задают материал. Нативная валидация рецепта
--- гарантирует шесть hex при активности; фолбэки — только защита
--- рендера, не активация. Свежие на каждый вызов.
function M.kumihimoTokens()
  local s = skinSettings()
  return {
    gap = 10,
    pad = 12,
    colors = {
      surface = hexColor(type(s) == "table" and s.surface or nil, "#241c1d"),
      surfaceVariant = hexColor(type(s) == "table" and s.surface_variant or nil, "#302326"),
      text = hexColor(type(s) == "table" and s.text or nil, "#e7d7bc"),
      muted = hexColor(type(s) == "table" and s.muted or nil, "#b5a38f"),
      accent = hexColor(type(s) == "table" and s.accent or nil, "#c28c59"),
      outline = hexColor(type(s) == "table" and s.outline or nil, "#70514b"),
    },
  }
end

--- Токены активного материала Stagecraft. Цвета — семантические роли хоста
--- как строки: реконсилютор резолвит их в актуальный цвет базовой палитры
--- на каждом кадре, без рецептов, замороженных значений и одноразовых
--- снимков getColor. Свежие на каждый вызов: одно чтение plugin_settings
--- на вызов, кэша между снапшотами нет.
function M.tokens()
  return {
    chamfer = 14,
    ring = 2,
    gap = 10,
    pad = 16,
    colors = {
      surface = "surface",
      surfaceVariant = "surface_variant",
      text = "on_surface",
      muted = "on_surface_variant",
      accent = "primary",
      outline = "outline",
    },
  }
end

--- Активен ли скин: плагин q/skin-runtime включён в [plugins].enabled,
--- семья — "stagecraft" (отсутствующий override читается как манифестный
--- дефолт; явно другая семья — неактивно), И опубликован валидный
--- applied_variant (32 строчных hex) — variant_id реально применённого
--- рецепта; SkinService снимает его при очистке, дефолт "" активностью
--- не считается, при невалидной проекции ключ маскируется нативно.
--- Один чтение plugin_settings на вызов (afterreload/settings обновляют
--- без кэша).
function M.stagecraft()
  if noctalia == nil or noctalia.getSetting == nil then
    return false
  end
  local ok, enabled = pcall(noctalia.getSetting, "plugins.enabled")
  if not ok or type(enabled) ~= "table" then
    return false
  end
  local present = false
  for _, id in ipairs(enabled) do
    if id == "q/skin-runtime" then
      present = true
      break
    end
  end
  if not present then
    return false
  end
  -- Отсутствующий override семьи — это манифестный дефолт "stagecraft":
  -- чистое применение с неизменённой семьёй читается как nil (элитые
  -- дефолты в конфиг не попадают). Отсекаем только явно другую семью;
  -- активность в любом случае решает applied_variant.
  local s = skinSettings()
  local family = type(s) == "table" and s.family or nil
  if family ~= nil and family ~= "stagecraft" then
    return false
  end
  return appliedVariant(s)
end

--- Карточка материала. Активно: material="stagecraft" (нативный рельефный
--- периметр, один rect-draw), shape="chamfer", заливка role surface;
--- opts.focused (узел реально держит фокус) — цвет рамки role primary
--- и ring-толщина, иначе цвет рамки role outline: цвет единого каркаса,
--- а не второй тонкий контур. Неактивно: та же колонка на обычных
--- палитровых ролях без material (отсутствие свойства сбрасывает
--- нативный Material в None), поэтому очистка скина возвращает обычный
--- вид без отдельной ветки.
--- Режимы используют разные суффиксы key: переключение active↔inactive
--- создаёт новое дерево вместо ретейна старых fill/border/shape на том
--- же узле (reconciler чистит их только явным новым значением).
--- opts: key, gap, padding, fill, shape, radius, focused, align,
--- flexGrow, width, height, minWidth, tooltip, border, borderWidth.
function M.card(props, children)
  props = props or {}
  local t = M.tokens()
  local active = M.stagecraft()
  local focused = props.focused == true and active
  local key = props.key
  if type(key) == "string" and key ~= "" then
    key = key .. (active and "-sc" or "-plain")
  end
  if active then
    -- Рельефный периметр рисует нативный материал; border/borderWidth —
    -- только цвет/толщина того же каркаса (фокус меняет цвет, не контур).
    return ui.column({
      key = key,
      gap = props.gap or t.gap,
      padding = props.padding or t.pad,
      fill = props.fill or t.colors.surface,
      shape = props.shape or "chamfer",
      radius = props.radius or t.chamfer,
      material = "stagecraft",
      align = props.align,
      flexGrow = props.flexGrow,
      width = props.width,
      height = props.height,
      minWidth = props.minWidth,
      tooltip = props.tooltip,
      border = props.border or (focused and t.colors.accent or t.colors.outline),
      borderWidth = props.borderWidth or (focused and t.ring or 1),
    }, children)
  end
  -- Ретейнутый узел с тем же key хранит старые fill/border/gap/padding:
  -- reconciler чистит их только по явному новому значению. Поэтому
  -- неактивная карточка отдаёт прозрачные/нулевые значения явно, а не nil.
  return ui.column({
    key = key,
    gap = props.gap or 0,
    padding = props.padding or 0,
    fill = props.fill or "surface/0",
    shape = props.shape,
    radius = props.radius,
    align = props.align,
    flexGrow = props.flexGrow,
    width = props.width,
    height = props.height,
    minWidth = props.minWidth,
    tooltip = props.tooltip,
    border = props.border or "outline/0",
    borderWidth = props.borderWidth or 0,
  }, children)
end

local ASSETS = "skin-assets/"
--: Ассеты семьи Kumihimo живут в подкаталоге материала.
local KUMIHIMO_ASSETS = ASSETS .. "kumihimo/"

--: Высота плетёного шнура-связки, логические px. Тонкая тесьма: шнур
--: протягивается по верхней кромке рамки и по рядам-полкам, но высоту
--: не наращивает — сам шнур лежит в строке ФИКСИРОВАННОЙ высоты.
M.KUMIHIMO_CORD = 8
--: Зазор между шнуром и содержимым внутри рамки Kumihimo, логические px.
--: Верхний бюджет рамки = pad + KUMIHIMO_CORD + KUMIHIMO_FRAME_GAP = pad+12;
--: потребитель выводит отсюда свой ringY (2*pad + 12), а не из магических чисел.
M.KUMIHIMO_FRAME_GAP = 4
--: Высота подвеса ярлыка (шнур+узел) над телом, логические px.
--: ФИКСИРОВАНА: фокус не меняет высоту подвеса, поэтому тело ярлыка
--: получает стабильный бюджет. Отогнутый угол (KUMIHIMO_FOLD) в подвес
--: больше НЕ кладётся: у coverFold он живёт в правом верхнем углу тела.
M.KUMIHIMO_HANGER = 14
--: Размер отогнутого угла-курсора И 45°-среза угла тела (shape="chamfer":
--: радиус = размер среза) — клетка клина фокуса и срез совпадают.
M.KUMIHIMO_FOLD = 14

--- Пассивный декоративный шнур: строка ФИКСИРОВАННОЙ высоты, внутри
--- которой картинка шнура растёт ТОЛЬКО по ширине, а сама строка роста
--- по главной оси не запрашивает. Прямой вызывающий в колонке (рамка,
--- полка, транспорт) обязан НЕ передавать flexGrow: иначе шнур занимал
--- бы остаток главной оси колонки и превращался в вертикальную ленту.
--- props.flexGrow — рост СТРОКИ по главной оси РОДИТЕЛЯ; он допустим
--- только когда вызывающий кладёт строку в горизонтальный ряд.
--- В обычной колонке строка и без flexGrow занимает всю её ширину:
--- ui-ряды выравнивают детей по поперечной оси (stretch).
--- props.joint=true добавляет маленький ограниченный узел на конце
--- шнура (стык-завязка, клетка квадратная — пропорция ассета не искажена).
--- key, height(default KUMIHIMO_CORD), width, flexGrow, joint.
function M.kumihimoCord(props)
  props = props or {}
  local height = props.height or M.KUMIHIMO_CORD
  local children = {}
  if props.joint == true then
    table.insert(children, ui.image({
      key = props.key and (props.key .. "-knot") or nil,
      path = KUMIHIMO_ASSETS .. "knot.png",
      width = height,
      height = height,
      fit = "stretch",
    }))
  end
  table.insert(children, ui.image({
    key = props.key and (props.key .. "-img") or nil,
    path = KUMIHIMO_ASSETS .. "cord.png",
    height = height,
    flexGrow = 1,
    fit = "stretch",
  }))
  return ui.row({
    key = props.key,
    gap = 0,
    width = props.width,
    height = height,
    flexGrow = props.flexGrow,
  }, children)
end

--- Подвешенный тёмный ярлык: декоративный подвес (шнур+узел), под ним
--- тело — единственная цель нажатия (onClick/onHover/заливка/рамка).
--- Декор не перехватывает клик и не захватывает клавиатуру.
---   active  — акцент навигации, отдельно от фокуса;
---   focused — фокус: тёплая рамка, а с coverFold — отогнутый угол;
---   hovered — локальный отклик указателя: теплеет ТОЛЬКО рамка тела
---             (заливка остаётся материальной), геометрия не двигается;
---   onHover — имя коллбэка ТЕЛА. Реконсилютор зовёт fn(state, key),
---             где state — "true"/"false", key — ключ узла тела
---             (key.."-body"), поэтому один хендлер обслуживает весь
---             keyed-ряд ярлыков; наведение на один ярлык не светит группу;
---   playing — фонарь слева от подвеса, никогда не подразумевает фокус;
---   coverFold — opt-in: стабильный слот KUMIHIMO_FOLD в правом верхнем
---             углу ТЕЛА (не подвеса). Слот занимает свою высоту ВСЕГДА,
---             независимо от фокуса, поэтому фокус не двигает содержимое;
---             мелкие вкладки и строки очереди слот не тратят.
--- Тело — тёмная карточка со срезанными углами (shape="chamfer",
--- radius=KUMIHIMO_FOLD): 45°-срез и клетка отогнутого угла совпадают,
--- поэтому клин фокуса ложится ровно в срез.
--- Про padding: Flex НЕ обрезает детей по срезанному силуэту, поэтому
--- содержимое остаётся внутри среза только при padding >= 7
--- (2*padding >= KUMIHIMO_FOLD). Мелкий ярлык либо паддится не меньше 7,
--- либо его углы выходят за силуэт тела.
--- height — ПОЛНАЯ высота ярлыка ВКЛЮЧАЯ подвес. Бюджет детей:
---   height - KUMIHIMO_HANGER - (coverFold and KUMIHIMO_FOLD or 0) - 2*padding
---   (зазор между детьми — props.gap; рамка в потоке высоту не отнимает).
--- key,width,height,gap,padding,fill,border,borderWidth,onClick,onHover,
--- hovered,active,focused,playing,coverFold.
function M.kumihimoTag(props, children)
  props = props or {}
  local t = M.kumihimoTokens()
  local key = props.key
  if type(key) ~= "string" or key == "" then
    key = "kumihimo-tag"
  end
  local active = props.active == true
  local focused = props.focused == true
  local hovered = props.hovered == true
  local playing = props.playing == true
  local coverFold = props.coverFold == true
  local hanger = M.KUMIHIMO_HANGER
  local fold = M.KUMIHIMO_FOLD
  local gap = props.gap or t.gap
  local padding = props.padding or t.pad
  -- Локальная подача указателя/фокуса — только ЦВЕТ и только рамка:
  -- заливка тела остаётся материальной константой (t.colors.surfaceVariant),
  -- поэтому ни hover, ни фокус не меняют ни геометрию, ни базовую заливку;
  -- явные props.fill/props.border вызывающего не перебиваются вообще.
  local fill = props.fill or t.colors.surfaceVariant
  local border = props.border or ((active or focused or hovered) and t.colors.accent or t.colors.outline)
  local borderWidth = props.borderWidth or 1
  -- Подвес: строка ФИКСИРОВАННОЙ высоты hanger. Ассеты-детали — в
  -- естественной пропорции (96x80 => 1.2): маленькие ограниченные стыки,
  -- а не растянутые в квадрат картинки.
  local left = {}
  if playing then
    table.insert(left, ui.image({
      key = key .. "-lamp",
      path = KUMIHIMO_ASSETS .. "lantern.png",
      width = 12,
      height = 10,
      fit = "stretch",
    }))
  end
  local bodyProps = {
    key = key .. "-body",
    flexGrow = 1,
    fill = fill,
    border = border,
    borderWidth = borderWidth,
    shape = "chamfer",
    radius = fold,
    onClick = props.onClick,
    onHover = props.onHover,
  }
  local body
  if coverFold then
    -- Слот угла — собственная строка тела: он присутствует всегда, а
    -- меняется только прозрачность клина, поэтому фокус не сдвигает
    -- содержимое. Дети живут в отдельной колонке со своими gap/padding,
    -- так что семантика props.padding/props.gap не меняется.
    bodyProps.gap = 0
    bodyProps.padding = 0
    body = ui.column(bodyProps, {
      ui.row({ key = key .. "-foldrow", gap = 0, height = fold, justify = "end", align = "center" }, {
        ui.image({
          key = key .. "-fold",
          path = KUMIHIMO_ASSETS .. "fold.png",
          width = fold,
          height = fold,
          fit = "stretch",
          opacity = focused and 1.0 or 0.0,
        }),
      }),
      ui.column({
        key = key .. "-cover",
        gap = gap,
        padding = padding,
        flexGrow = 1,
      }, children or {}),
    })
  else
    bodyProps.gap = gap
    bodyProps.padding = padding
    body = ui.column(bodyProps, children or {})
  end
  return ui.column({
    key = key,
    gap = 0,
    width = props.width,
    height = props.height,
    flexGrow = props.flexGrow,
  }, {
    ui.row({ key = key .. "-hang", gap = 0, height = hanger, align = "center" }, {
      ui.row({ key = key .. "-hang-l", gap = 0, flexGrow = 1, justify = "start", align = "center" }, left),
      ui.image({
        key = key .. "-hanger",
        path = KUMIHIMO_ASSETS .. "hanger.png",
        width = 17,
        height = hanger,
        fit = "stretch",
      }),
      ui.row({ key = key .. "-hang-r", gap = 0, flexGrow = 1, justify = "end", align = "center" }, {}),
    }),
    body,
  })
end

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

--- Рамка blossom-weave (неактивный скин).
-- Середина (рельсы + содержимое) — единственный гибкий узел дерева.
function M.blossomFrame(content, key)
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

--- Рамка в потоке разметки. Неактивно — blossom-weave (как раньше).
--- Активно — одна нативная колонка material="stagecraft" (рельефный
--- периметр в одном rect-draw), shape="chamfer": заливка role surface,
--- цвет рамки role outline — role primary при opts.focused; это цвет
--- единого каркаса, без второго тонкого контура. Бюджет потока
--- эквивалентен прежнему (padding EDGE→pad с каждой стороны).
--- Режимы используют разные суффиксы key: переключение active↔inactive
--- создаёт новое дерево вместо ретейна старых fill/border/chamfer на том
--- же узле (reconciler чистит их только явным новым значением).
--- @param content узел содержимого панели
--- @param key стабильный префикс ключей
--- @param opts nil или { focused = bool }
function M.frame(content, key, opts)
  opts = opts or {}
  -- Kumihimo: НЕ blossom (не толстая «деревянная» рамка) и НЕ
  -- Stagecraft-материал. Лёгкий тёмный периметр со СКРУГЛЁННЫМИ углами
  -- (как у референсной панели, radius 10) с авторским шнуром
  -- вдоль верхней кромки: шнур-акцент на стыке, а не каркас на каждой
  -- области. Срезанный угол (shape="chamfer") — язык подвесных ярлыков
  -- Kumihimo, а НЕ всего периметра панели: рамка остаётся скруглённой.
  -- Шнур лежит в строке ФИКСИРОВАННОЙ высоты KUMIHIMO_CORD и
  -- растёт только по ширине — в колонке рамки flexGrow ему НЕ передаётся,
  -- иначе он занял бы остаток вертикали. Верхний бюджет рамки:
  -- pad + KUMIHIMO_CORD + KUMIHIMO_FRAME_GAP = pad + 12 (отсюда у tmus
  -- ringY = 2*pad + 12). Суффикс key "-kumi" изолирует дерево при
  -- переключении/очистке семьи.
  if M.kumihimo() then
    local t = M.kumihimoTokens()
    if type(key) ~= "string" or key == "" then
      key = "kumihimo"
    end
    key = key .. "-kumi"
    return ui.column({
      key = key,
      flexGrow = 1,
      gap = M.KUMIHIMO_FRAME_GAP,
      padding = t.pad,
      fill = t.colors.surface,
      border = t.colors.outline,
      borderWidth = 1,
      radius = 10,
    }, {
      M.kumihimoCord({ key = key .. "-cord", joint = true }),
      content,
    })
  end
  if not M.stagecraft() then
    return M.blossomFrame(content,
      type(key) == "string" and key ~= "" and key .. "-legacy" or nil)
  end
  local t = M.tokens()
  if type(key) ~= "string" or key == "" then
    key = "stagecraft"
  end
  return ui.column({
    key = key,
    flexGrow = 1,
    gap = 0,
    padding = t.pad,
    shape = "chamfer",
    radius = t.chamfer,
    material = "stagecraft",
    fill = t.colors.surface,
    border = opts.focused == true and t.colors.accent or t.colors.outline,
    borderWidth = opts.focused == true and t.ring or 1,
  }, { content })
end

return M
