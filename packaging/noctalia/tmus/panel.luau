--!nonstrict

local KEY = "tmus"
local SERVICE = "q/tmus:watch"

-- Общий скин десктопа: модуль и skin-assets кладутся рядом с плагином
-- при деплое (раскладку ведёт оркестратор). Свой модуль НЕ заводим —
-- контракт один на все панели, иначе скины разъедутся. Локальный
-- require требует plugin_api >= 22; у плагина 32 (минимум модуля —
-- 22, плюс getSetting 26 и getColor 31, см. комментарий манифеста).
local skin = require('./desktop_skin.luau')
local Hint = require('./hint_navigation.luau')
local hints = Hint.new()
local volumeKeys = false

-- Активна ли семья Stagecraft в текущем кадре и её рез шамфера (канон
-- skin.tokens().chamfer). Считается ОДИН раз в начале draw(), а не в
-- каждом узле: у панели десятки строк, а stagecraft()/tokens() читают
-- настройки. Смена рецепта подхватывается на ближайшем рендере.
local SC = false
local CHAM = 14

-- Семья Kumihimo текущего кадра и её токены (шесть цветов применённого
-- рецепта). Взаимоисключающа со SC: у активного Kumihimo stagecraft()
-- вернёт false. Тоже читается один раз на кадр.
local KU = false
local KT = nil

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
local COLORS_ROLE = {
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
  -- Тёплый графит хрома (шапка, сайдбар): карточки контента остаются на
  -- surface, чтобы рабочая область читалась как содержимое, а не как
  -- ещё одна плашка. Роль с альфой — из проверенно рендерящихся.
  chrome = "surface_variant/0.55",
}

-- Активная палитра кадра. Роли хоста (Stagecraft/дефолт) ЛИБО hex-цвета
-- применённого рецепта Kumihimo: у Kumihimo материал задают обои, а не
-- базовая палитра, поэтому роли тут не участвуют. Значения читаются на
-- каждом использовании, поэтому подмена полей в начале draw() перекрашивает
-- весь кадр без правок десятков узлов.
local COLORS = {}
-- Новые рендер-хелперы Kumihimo живут полями одной таблицы: у главного
-- чанка жёсткий лимит локальных имён, и каждая отдельная `local function`
-- занимает слот до конца файла.
local KUF = {}
function KUF.rolePalette()
  for k, v in pairs(COLORS_ROLE) do
    COLORS[k] = v
  end
end

-- Палитра Kumihimo из шести настроек применённого рецепта. Полупрозрачные
-- заливки — hex с альфой "#rrggbbaa" (парсер цвета хоста её принимает).
-- selected(курсор) и playing(играет) — производные акцента и muted, НЕ
-- декоративный шнур: бордо остаётся материалом, а не идентичностью
-- фокуса/воспроизведения.
function KUF.kumihimoPalette(t)
  local c = t.colors
  COLORS.bg = c.surface
  COLORS.surface = c.surfaceVariant
  COLORS.selected = c.accent .. "47"
  COLORS.playing = c.muted .. "33"
  COLORS.npbg = c.surfaceVariant .. "8c"
  COLORS.text = c.text
  COLORS.muted = c.muted
  COLORS.border = c.outline
  COLORS.accent = c.accent
  COLORS.error = "#c2504a"
  COLORS.chrome = c.surfaceVariant .. "8c"
end
KUF.rolePalette()

-- Тёплая локальная палитра кнопки Kumihimo: явные normal/hover/pressed
-- цвета держат состояния кнопки в материале рецепта и не дают хост-синему
-- (variant primary/ghost) просвечивать. p.hot — акцентная кнопка
-- (toggle/включённый режим): заливка accent, текст/глиф — surface. В
-- неактивной семье props возвращаются нетронутыми: кнопка остаётся
-- ровно на прежней variant-палитре, дефолт не меняется.
function KUF.warmButton(p, hot)
  if not KU then
    return p
  end
  local c = KT.colors
  if hot then
    p.fill = c.accent
    p.hoverFill = c.accent
    p.pressedFill = c.accent
    p.color = c.surface
    p.hoverColor = c.surface
    p.pressedColor = c.surface
  else
    p.fill = c.surfaceVariant
    -- Полупрозрачный accent поверх тёплой поверхности: hover/press
    -- только меняют локальный цвет, геометрия не двигается.
    p.hoverFill = c.accent .. "40"
    p.pressedFill = c.accent .. "66"
    p.color = c.text
    p.hoverColor = c.text
    p.pressedColor = c.text
  end
  p.border = c.outline
  p.borderWidth = 1
  p.radius = 6
  return p
end

-- Kumihimo-toggle: тёплый ОЧЕРЧЕННЫЙ круг 32×32 (radius 16) с акцентной
-- рамкой и акцентным глифом — главная кнопка транспорта читается, но не
-- становится сплошной золотой плашкой. Кнопка-глиф и так меряется 32×32
-- (глиф 16 + паддинг 8×2), поэтому явные width/height лишь фиксируют
-- круг, а contentAlign центрирует глиф в нём. Вне Kumihimo props
-- возвращаются нетронутыми: non-KU вид не меняется.
function KUF.warmToggleProps(p)
  if not KU then
    return p
  end
  local c = KT.colors
  KUF.warmButton(p, false)
  p.border = c.accent
  p.color = c.accent
  p.hoverColor = c.accent
  p.pressedColor = c.accent
  p.radius = 16
  p.width = 32
  p.height = 32
  p.contentAlign = "center"
  return p
end

-- UI-текст — sans скина (Noto Sans); MONO (Agave Nerd Font) остаётся
-- глифам и числовым метаданным: тайминги, счётчики, индексы, полосы eq,
-- индикатор окна. Кириллица есть и там, и там (Noto Sans CJK — фолбэк
-- хоста), поэтому переносов и «тофу» не будет.
local FONT = skin.FONT
local RAIL = 64
local GAP = 12
local PAD = 16
-- Хост режет манифестную площадь панели ДО plugin.layout: native
-- panel_manager вычитает 2*Style::panelPadding (14) с каждой оси, поэтому
-- 1180×760 из манифеста — внешний размер, а рисовать панель может в
-- 1152×732. Без этой поправки бюджеты разъезжаются с фактом на 28 px по
-- обеим осям (окно считалось по 468, а вьюпорт был 440).
local HOST_PANEL_PAD = 14
-- ScrollView при переполнении контента резервирует под полосу прокрутки
-- scrollbarWidth(6)+scrollbarGap(8). Ширину карточек home считаем с этим
-- резервом ЗАРАНЕЕ, чтобы третья обложка не лезла под полосу.
local SCROLL_GUTTER = 14
-- ── Геометрия панели и бюджет окон ──────────────────────────────────
-- Панель 1180×760 (plugin.toml). Рамка скина резервирует EDGE=12 px
-- с КАЖДОЙ стороны прямо в потоке (2*EDGE по оси); у активного Kumihimo
-- периметр тонкий, но сверху ещё шнур-акцент, поэтому ringX/ringY
-- считаются раздельно. Сверх рамки идут собственный отступ (PAD),
-- шапка/вкладки, футер и транспортная карточка. Бюджеты ниже ВЫВЕДЕНЫ
-- из этой геометрии, а не назначены: окно рендерит ровно столько строк
-- и карточек, сколько влезает в реальный вьюпорт — иначе курсор мог бы
-- уехать на невидимую строку.
local GEO = {
  pad = PAD,
  ringX = skin.EDGE * 2,
  ringY = skin.EDGE * 2,
  header = 26,
  footer = 16,
  -- Транспорт (np-карточка): паддинг 12×2 + арт-строка 48 + спектр 18 +
  -- прогресс 4 + два зазора по 8. В дефолте — последний ребёнок
  -- workspace; у Kumihimo вынесен под main во всю ширину.
  transport = 110,
  panelW = 1180,
  panelH = 760,
}

-- Верхняя строка транспорта Kumihimo — ОДНА раскладка на оба состояния
-- (с треком и без): левая зона left шириной leftW, правая right шириной
-- rightW, между ними (если передан) центр prev/toggle/next. Ширины зон
-- приходят из GEO — они посчитаны в recomputeLayout от фактической
-- ширины карточки, поэтому длинный заголовок/статус не распирает строку
-- и не выдавливает контролы за правый край. Хелпер только собирает уже
-- созданные узлы: обработчики, ключи и состояние — вызывающего.
-- Определение стоит ПОСЛЕ инициализатора local GEO: оба обращения
-- (npCenterW, npTopH) читают лексически связанную локальную таблицу.
function KUF.npTopRow(zones)
  local row = {
    ui.row({ key = "np-left", gap = 12, align = "center", justify = "start", width = zones.leftW }, zones.left),
    ui.row({ key = "np-right", gap = 12, align = "center", justify = "end", width = zones.rightW }, zones.right),
  }
  if zones.center then
    table.insert(
      row,
      2,
      ui.row({ key = "np-controls", gap = 6, align = "center", width = GEO.npCenterW }, zones.center)
    )
  end
  return ui.row({ key = "np-top", gap = 12, align = "center", height = GEO.npTopH }, row)
end
local GRID_COLS = 4
local NP_SPEC_BARS = 56
-- Бюджеты, зависящие от семьи: высоты рядов/полок и выведенные окна.
-- Пересчитываются каждый кадр в recomputeLayout (смена семьи/рецепта
-- подхватывается на ближайшем draw).
local TRACK_ROW_HEIGHT, TRACK_ROW_GAP = 44, 6
local CARD_ROW_HEIGHT, CARD_ROW_GAP = 200, GAP
local QUEUE_ROW_HEIGHT, QUEUE_ROW_GAP = 36, 4
local HOME_SHELF_HEIGHT = 210
local HOME_CARD_WIDTH = 150
local LIST_WINDOW, CARD_WINDOW, QUEUE_WINDOW, SHELF_WINDOW, HOME_CARD_WINDOW = 0, 0, 0, 0, 0

-- Единственный источник бюджетов. non-Kumihimo ветка В ТОЧНОСТИ
-- повторяет прежние числа; Kumihimo меняет шапку на горизонтальные
-- вкладки-подвесы, выносит транспорт во всю ширину, убирает левый рейл
-- и делает ряды очереди/полки выше — поэтому вьюпорты другие.
-- kuView — ТЕКУЩИЙ вид: функция объявлена до `local view`, поэтому вид
-- приходит параметром, а не захватом позже объявленной локальной
-- переменной (та молча стала бы глобалом/nil).
function KUF.recomputeLayout(ku, kuPad, kuView)
  if ku then
    local pad = kuPad or 12
    -- Шнур/зазор рамки — из shared-модуля, а не дрейфующая копия:
    -- бюджет не должен разъезжаться с фактическим узлом frame().
    local cord = skin.KUMIHIMO_CORD
    local frameGap = skin.KUMIHIMO_FRAME_GAP
    -- Плагин рисует уже ОБРЕЗАННУЮ хостом площадь: 1180×760 из манифеста
    -- минус 2*HOST_PANEL_PAD по каждой оси.
    local panelW = GEO.panelW - 2 * HOST_PANEL_PAD
    local panelH = GEO.panelH - 2 * HOST_PANEL_PAD
    -- Корень Kumihimo паддится неодинаково: 16 по горизонтали, 8 по
    -- вертикали. Оба числа участвуют в бюджете тела.
    local padH, padV = 16, 8
    -- Вкладки-подвесы: подвес 14 + тело 34 (паддинг 7×2 + строка 20).
    -- Паддинг тела обязан быть >= 7: у срезанного угла chamfer катет равен
    -- KUMIHIMO_FOLD (14), и контент с паддингом < 7 выступал бы за срез
    -- (Flex не режет детей по силуэту). 48 = HANGER + 2*7 + строка глифа.
    GEO.header = 48
    -- np-карточка: паддинг 12×2 + арт-строка 48 + два зазора 8 + прогресс
    -- 4 + спектр 18 = 110 ровно. Высота карточки задаётся ЯВНО (npNode),
    -- поэтому бюджет и факт совпадают и карточка не прыгает.
    GEO.npcH = 110
    GEO.npTopH = 48
    -- Fixed-height шнур-ряд cord + зазор 6 + np-карточка.
    GEO.transport = cord + 6 + GEO.npcH
    GEO.ringX = 2 * pad
    -- Периметр pad×2 + верхний шнур cord + зазор рамки frameGap.
    GEO.ringY = 2 * pad + cord + frameGap
    -- Три зазора корня: вкладки·тело, тело·транспорт, транспорт·футер;
    -- сам транспорт — отдельный сиблинг main во всю ширину, поэтому его
    -- высота тоже вычитается из бюджета тела. По вертикали корень
    -- паддится padV (8), по горизонтали — padH (16).
    GEO.bodyH = panelH - GEO.ringY - 2 * padV - GEO.header - GEO.footer
      - 3 * GAP - GEO.transport
    GEO.bodyW = panelW - GEO.ringX - 2 * padH
    -- Транспорт вынесен под main: тело целиком идёт центральному виду.
    GEO.viewH = GEO.bodyH
    -- Левого рейла нет: ширину делят только workspace:context — 7:3, а в
    -- поиске 63:37 (ровно как flexGrow в main), поэтому доля приходит
    -- параметром вида, а не выводится из фиксированной константы.
    local wsNum, wsDen = 7, 10
    if kuView == "search" then
      wsNum, wsDen = 63, 100
    end
    GEO.workspaceW = math.floor((GEO.bodyW - GAP) * wsNum / wsDen)
    -- ── Бюджет строки транспорта ────────────────────────────────────
    -- Карточка занимает всю ширину тела; её контент — минус паддинг 12.
    -- Ширины трёх зон ВЫВЕДЕНЫ из этой ширины, а не набраны flexGrow с
    -- натуральной длиной текста: иначе 48-символьный заголовок или
    -- длинный статус распирали бы строку и выдавливали контролы,
    -- громкость и время за правый край.
    local npW = GEO.bodyW - 2 * 12
    -- Центр: prev/toggle/next — три круга 32 и два зазора 6.
    local npCenterW = 3 * 32 + 2 * 6
    -- Справа: строка громкости (глиф 14 + зазор 6 + слайдер 96 — та же
    -- ширина, что задаёт volumeNode, поэтому она в GEO) + зазор 12 +
    -- колонка времени/статуса (моно 12: «88:88 / 88:88» ≈ 94; 150 — с
    -- запасом на строку статуса, который при переполнении эллипсится).
    local npTimeW = 150
    local npVolW = 14 + 6 + 96
    -- Равные боковые зоны удерживают prev/toggle/next точно в центре.
    -- Справа содержимое прижато к краю; слева текст получает остаток
    -- после арта и двух режимов, не вытесняя центр.
    local npLeftW = (npW - npCenterW - 2 * 12) / 2
    local npRightW = npLeftW
    GEO.npW = npW
    GEO.npCenterW, GEO.npRightW, GEO.npTimeW = npCenterW, npRightW, npTimeW
    GEO.npVolW = npVolW
    GEO.npLeftW = npLeftW
    GEO.npTextW = npLeftW - (48 + 12 + 12 + 3 * 32 + 3 * 6 + 20)
    -- Без трека центра нет: правая зона вбирает и его ширину.
    GEO.npStateW = npRightW + npCenterW + 12
    TRACK_ROW_HEIGHT, TRACK_ROW_GAP = 44, 6
    CARD_ROW_HEIGHT, CARD_ROW_GAP = 200, GAP
    -- Подвесной ряд очереди: подвес HANGER 14 + тело 44 при паддинге 7 —
    -- контент (арт 28) получает 58−14−14 = 30, запас 2. Паддинг >=7
    -- обязателен: иначе контент выступает за chamfer-срез угла.
    QUEUE_ROW_HEIGHT, QUEUE_ROW_GAP = 58, 6
    -- Подвесная карточка обложки (coverFold=true): подвес HANGER 14 +
    -- стабильный слот угла FOLD 14 + тело 152 = 180. Слот угла ВЫРЕЗАЕТСЯ
    -- из высоты, детям тела (padding 7) остаётся 180 − 14 − 14 − 2×7 = 138.
    -- slot: квадратный арт 96 + caption-обёртки 18/16 + два зазора 3 = 136
    -- (нативные Label.height не применяются — фиксированные слоты держат
    -- flex-обёртки, а не сам label). padding 7 — минимум против chamfer.
    GEO.cardH = 180
    GEO.cardArt = 96
    -- Полка: заголовок 20 + зазор 6 + шнур 8 + зазор 0 + карточка 180 =
    -- 214. Две полки (2×214 + GAP 12 = 440) влезают в вьюпорт 456.
    HOME_SHELF_HEIGHT = 20 + 6 + cord + 0 + GEO.cardH
    -- Обложка: ширина ВЫВЕДЕНА из фактической ширины рабочей области.
    -- Из неё заранее вычтен SCROLL_GUTTER (полоса прокрутки), поэтому три
    -- обложки и два зазора (3×W + 2×GAP) гарантированно влезают даже
    -- когда полоса прокрутки появится.
    HOME_CARD_WIDTH = math.floor((GEO.workspaceW - SCROLL_GUTTER - 2 * GAP) / 3)
  else
    GEO.header = 26
    GEO.transport = 110
    GEO.ringX = skin.EDGE * 2
    GEO.ringY = skin.EDGE * 2
    GEO.bodyH = GEO.panelH - GEO.ringY - 2 * GEO.pad - GEO.header - GEO.footer - 2 * GAP
    GEO.bodyW = GEO.panelW - GEO.ringX - 2 * GEO.pad
    GEO.viewH = GEO.bodyH - (GEO.transport + GAP)
    GEO.workspaceW = math.floor((GEO.bodyW - (RAIL + GAP) - GAP) * 7 / 10)
    TRACK_ROW_HEIGHT, TRACK_ROW_GAP = 44, 6
    CARD_ROW_HEIGHT, CARD_ROW_GAP = 200, GAP
    QUEUE_ROW_HEIGHT, QUEUE_ROW_GAP = 36, 4
    HOME_SHELF_HEIGHT = 210
    HOME_CARD_WIDTH = 150
  end
  -- Над списком стоит заголовок вида (24) и зазор; вьюпорт — от него.
  local trackVp = GEO.viewH - (24 + GAP)
  LIST_WINDOW = math.floor((trackVp + TRACK_ROW_GAP) / (TRACK_ROW_HEIGHT + TRACK_ROW_GAP))
  CARD_WINDOW = GRID_COLS * math.floor((trackVp + CARD_ROW_GAP) / (CARD_ROW_HEIGHT + CARD_ROW_GAP))
  -- Колонка очереди: тело минус собственный паддинг, заголовок (20), зазор.
  local queueVp = GEO.bodyH - 2 * GEO.pad - (20 + GAP)
  QUEUE_WINDOW = math.floor((queueVp + QUEUE_ROW_GAP) / (QUEUE_ROW_HEIGHT + QUEUE_ROW_GAP))
  -- Полки home: у Kumihimo дублирующего трек-хиро нет — лента начинается
  -- сразу, поэтому вьюпорт полок равен полной высоте вида; у дефолта над
  -- полками висит хиро (64 + зазор). Окно Kumihimo дополнительно капнуто
  -- самой композицией: не больше двух полок и трёх обложек в ряду, даже
  -- если выведенное по ширине/высоте окно шире.
  local shelfVp = GEO.viewH - (ku and 0 or (64 + GAP))
  SHELF_WINDOW = math.floor((shelfVp + GAP) / (HOME_SHELF_HEIGHT + GAP))
  HOME_CARD_WINDOW = math.floor((GEO.workspaceW + GAP) / (HOME_CARD_WIDTH + GAP))
  if ku then
    SHELF_WINDOW = math.min(2, SHELF_WINDOW)
    HOME_CARD_WINDOW = math.min(3, HOME_CARD_WINDOW)
    -- Clients: реальный провайдерский ряд 64 при зазоре 6; бюджет окна
    -- выводится из этих чисел, а не из общей строки списка.
    GEO.clientRow, GEO.clientGap = 64, 6
  end
end
KUF.recomputeLayout(false)

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
  exclam = "!", quotedbl = '"', numbersign = "#", dollar = "$",
  percent = "%", ampersand = "&", parenleft = "(", parenright = ")",
  asterisk = "*", plus = "+", slash = "/", colon = ":", semicolon = ";",
  less = "<", equal = "=", greater = ">", question = "?", at = "@",
  bracketleft = "[", backslash = "\\", bracketright = "]", asciicircum = "^",
  underscore = "_", grave = "`", braceleft = "{", bar = "|", braceright = "}",
  asciitilde = "~", numerosign = "№",
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

local function textChar(chord)
  -- capture_keys delivers the declared chord verbatim, including Shift.
  if string.sub(chord, 1, 6) == "Shift+" then
    chord = string.sub(chord, 7)
    -- Latin capture names are uppercase even without Shift; only text mode
    -- interprets Shift as case. Cyrillic keysyms already encode their case.
    if #chord == 1 and chord >= "A" and chord <= "Z" then
      return chord
    end
  end
  return TEXT_CHARS[chord]
end
-- ── Редактор поиска: код-поинтовая арифметика ────────────────────────
-- Курсор queryCursor — ЧИСЛО КОД-ПОИНТОВ до каретки (0..len),
-- queryLeft — левый край видимого окна, тоже в код-поинтах. Все
-- мутации строки идут через utf8.offset — байтовый срез не может
-- разрезать кириллицу. Лимит буфера — по символам, не по байтам.
local QUERY_MAX_CHARS = 64
local SEARCH_VIEW_CHARS = 56

local function charCount(s)
  local n = utf8.len(s)
  return (n == nil or n < 0) and 0 or n
end

-- Байт-смещение начала код-поинта с индексом cp (0..len); cp == 0 —
-- начало строки, cp == len — позиция за последним символом.
local function byteOffset(s, cp)
  if cp <= 0 then
    return 1
  end
  return utf8.offset(s, cp + 1) or (#s + 1)
end

-- Все три хелпера возвращают (новая строка, новый курсор).
local function insertAt(s, cursor, text)
  local off = byteOffset(s, cursor)
  return string.sub(s, 1, off - 1) .. text .. string.sub(s, off), cursor + charCount(text)
end

local function deleteBefore(s, cursor)
  if cursor <= 0 then
    return s, 0
  end
  local a = byteOffset(s, cursor - 1)
  local b = byteOffset(s, cursor)
  return string.sub(s, 1, a - 1) .. string.sub(s, b), cursor - 1
end

local function deleteAt(s, cursor)
  if cursor >= charCount(s) then
    return s, cursor
  end
  local a = byteOffset(s, cursor)
  local b = byteOffset(s, cursor + 1)
  return string.sub(s, 1, a - 1) .. string.sub(s, b), cursor
end

-- ctrl+BackSpace: удалить слово СЛЕВА ОТ КУРСОРА, модель
-- readline/VSCode — срезать хвостовые разделители, затем слово, затем
-- снова разделители — на префиксе до каретки. ASCII-классы %s/%p не
-- пересекаются с байтами кириллицы (все ≥0x80), поэтому срез всегда
-- проходит по границе код-поинта. Автоповтор сознательно не
-- подключён: лаг одного кадра съел бы полстроки.
local function killWordBefore(s, cursor)
  local cursorOff = byteOffset(s, cursor)
  local head = string.sub(s, 1, cursorOff - 1)
  local suffix = string.sub(s, cursorOff)
  head = string.gsub(head, "[%s%p]+$", "")
  head = string.gsub(head, "[^%s%p]+$", "")
  head = string.gsub(head, "[%s%p]+$", "")
  return head .. suffix, charCount(head)
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
-- Курсор и левый край видимого окна поиска — в код-поинтах (0..len).
local queryCursor = 0
local queryLeft = 0
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
-- insert: Esc или пары jj/jk на физических клавишах (о/л на ЙЦУКЕН —
-- это те же j/k); вход: i, / или точка. Латинское «oo» эскейпом
-- БОЛЬШЕ НЕ является: обе «o» вставляются как текст.
local typing = false
local insertPending = nil
local searchEnterDown = false
local searchPasteDown = false
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
local maybeHomeMore -- defined below; used by home move/clamp (догрузка ленты)
local endHold -- defined below; used by hintable volume and search controls

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

-- Kumihimo-спектр: ровно 24 реальные полосы хоста (публикует виджет в
-- лёгкий ключ). Каждая полоса — grow-ребёнок ПОЛНОШИРИННОГО ряда, поэтому
-- никакой короткой левой ленты: полосы делят всю ширину транспорта.
-- Значения парсятся тем же CSV и клампятся в 0..1; нулевая полоса (тишина)
-- оставляет базовую линию 1 px. Ряд выравнивается по нижнему краю кадра.
function KUF.kuSpectrum(csv)
  local values = {}
  local at = 1
  for num in string.gmatch(tostring(csv or ""), "([^,]+)") do
    if at > 24 then
      break
    end
    values[at] = math.max(0, math.min(1, tonumber(num) or 0))
    at = at + 1
  end
  local bars = {}
  for i = 1, 24 do
    bars[i] = ui.box({
      key = "spec-bar" .. i,
      flexGrow = 1,
      height = math.max(1, math.floor((values[i] or 0) * 18 + 0.5)),
      radius = 1,
      fill = COLORS.accent,
    })
  end
  return ui.row({ key = "spec-bars", gap = 2, align = "end", flexGrow = 1 }, bars)
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

-- Провайдер из составного id "prov:id" (трек пикера/строки плейлиста).
local function trackProvider(idstr)
  return tostring(idstr or ""):match("^([^:]+):") or ""
end

-- Локальный провайдер: всегда поддерживает все плейлистные операции
-- (playlist_create/add/remove/delete) — это собственное хранилище
-- демона, capability-снимок для него не нужен.
local function isLocalProvider(pid)
  return tostring(pid or "") == "local"
end

-- Capability-снимок провайдера из snapshot.providers (сервис кладёт
-- caps в pullProviders). Неизвестный провайдер — nil: нативные жесты
-- для него запрещены (fail-closed), локальные доступны всегда.
local function capsOfProvider(pid)
  for _, p in ipairs(snapshot.providers or {}) do
    if tostring(p.id or "") == tostring(pid or "") then
      return p.caps
    end
  end
  return nil
end

-- Плейлист доступен как адресат «добавить трек»: локальный — всегда;
-- нативный — только когда провайдер плейлиста совпадает с провайдером
-- трека И capability-снимок говорит playlist_add. Иначе нативный
-- пункт в пикере не предлагается вовсе.
local function playlistAddable(p, trackProv)
  local prov = type(p.id) == "table" and tostring(p.id.provider or "") or ""
  if isLocalProvider(prov) then
    return true
  end
  if prov ~= trackProv then
    return false
  end
  local caps = capsOfProvider(prov)
  return type(caps) == "table" and caps.playlist_add == true
end

-- Рендер-модель home-ленты: массив полок {title, subtitle, count,
-- cards} из homeFeed. Держим все карточки для навигации; draw
-- рендерит только видимое окно. Пустые полки выбрасываются.
-- Перевычисляется только в watch-колбэке (rebuildHomeShelves) — draw
-- до 8 Гц не должен перебирать полки.
local function rebuildHomeShelves()
  local out = {}
  if type(homeFeed) == "table" and homeFeed.status == "ok" then
    for _, shelf in ipairs(homeFeed.shelves or {}) do
      local items = shelf.items or {}
      local cards = {}
      for _, it in ipairs(items) do
        local kind = tostring(it.kind or "")
        if kind == "track" then
          table.insert(cards, {
            kind = "track",
            id = idOf(it),
            title = tostring(it.title or ""),
            subtitle = type(it.artists) == "table" and table.concat(it.artists, ", ") or "",
            artist = type(it.artists) == "table" and table.concat(it.artists, ", ") or "",
            art = tostring(it.art_path or ""),
            time = durationLabel(it.duration),
            event = "radio",
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
          count = #cards,
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


local function lbl(text, props)
  local p = props or {}
  -- height/maxLines доходят до хоста как есть: label своей высоты не
  -- имеет (её задаёт строка/колонка родителя), а maxLines=1 запрещает
  -- перенос там, где бюджет строки посчитан на одну строку. nil — ключ
  -- не выставляется, поэтому не-Kumihimo вызовы не меняются.
  return ui.label({
    key = p.key,
    text = text,
    color = p.color or COLORS.text,
    fontSize = p.size,
    -- p.mono — числовые метаданные и знаки (▶ ♥ ◄ ► ▌ ×1 ∞): эти
    -- код-поинты гарантированно есть в Agave Nerd Font (skin.MONO).
    fontFamily = p.mono and skin.MONO or FONT,
    flexGrow = p.grow,
    width = p.width,
    height = p.height,
    maxWidth = p.maxWidth,
    maxLines = p.maxLines,
    opacity = p.opacity,
  })
end

-- Search consumes raw metadata; captions may contain badges or truncation.
local function searchLabel(text, props, value, identity)
  local queryText = noctalia.string.trim(tostring(value or ""))
  if queryText == "" then return lbl(text, props) end
  local label = lbl(text, props)
  label.props.flexGrow = 1
  label.props.maxLines = 1
  return ui.row({
    key = tostring(props.key) .. "-search",
    flexGrow = props.grow,
    width = props.width,
    height = props.height,
    align = "center",
    hintIdentity = tostring(identity) .. ":" .. queryText,
    onClick = function() submitSearch(queryText) end,
  }, { label })
end

local function hintTarget(node, identity, badge)
  local p = node.props or {}
  if type(p.onClick) == "function" then
    p.hintIdentity = identity
    p.hintAnchor = badge
    return node
  end
  for _, child in ipairs(node.children or {}) do
    if hintTarget(child, identity, badge) then return node end
  end
  return nil
end

local function renderPanel(tree)
  local rendered = hints:render(tree, COLORS.accent)
  panel.setCaptureCancel(hints:isActive() or hints:isStale())
  panel.render(rendered)
end

local function card(children, props)
  local p = props or {}
  -- Kumihimo: тёмная тёплая поверхность рецепта, тонкая рамка outline;
  -- НЕ Stagecraft-материал и не blossom. Явные p.fill/p.border сильнее.
  if KU then
    return ui.column({
      key = p.key,
      gap = p.gap or 10,
      padding = p.padding or PAD,
      radius = p.radius or 8,
      -- p.shape — opt-in (state-карточки KU): срезанный угол 8 совпадает
      -- с языком ярлыков; по умолчанию остаётся прежняя скруглённая форма.
      shape = p.shape,
      fill = p.fill or KT.colors.surfaceVariant,
      border = p.border or KT.colors.outline,
      borderWidth = p.borderWidth or 1,
      flexGrow = p.grow,
      width = p.width,
      -- Явная высота карточки Kumihimo: транспорт держит свой бюджет
      -- (GEO.npcH), а не натуральную сумму детей, иначе строка могла бы
      -- разойтись с посчитанным вьюпортом.
      height = p.height,
      maxWidth = p.maxWidth,
    }, children)
  end
  -- Материал Стейджикрафта: шамферная гравировка и серебряный волосок
  -- включаются ТОЛЬКО для активной семьи; для остальных (и для дефолта
  -- без скина) остаются прежняя скруглённая геометрия и отсутствие
  -- рамки. Явные p.radius/p.border вызывающего всегда сильнее.
  local sc = SC
  return ui.column({
    key = p.key,
    gap = p.gap or 10,
    padding = p.padding or PAD,
    radius = p.radius or (sc and CHAM or 10),
    shape = sc and "chamfer" or nil,
    fill = p.fill or COLORS.surface,
    border = p.border or (sc and COLORS.border or nil),
    borderWidth = p.borderWidth or (sc and 1 or nil),
    flexGrow = p.grow,
    width = p.width,
    maxWidth = p.maxWidth,
  }, children)
end

-- Дети декларативного узла обязаны быть плотной последовательностью.
-- compact(...) собирает варахарги без дыр: nil/false пропускаются, порядок
-- и все реальные узлы сохраняются. Счёт по select('#', ...), поэтому
-- результат не зависит от длины массива-источника.
local function compact(...)
  local out = {}
  local count = select("#", ...)
  for i = 1, count do
    local node = select(i, ...)
    if node ~= nil and node ~= false then
      out[#out + 1] = node
    end
  end
  return out
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
  local sliderProps = {
    key = "np-volume-slider",
    min = 0,
    max = 100,
    step = 5,
    value = current,
    width = 96,
    -- Kumihimo: точную ширину слайдер получает как grow-ребёнок строки
    -- громкости (см. volumeNode ниже) — проп width хост игнорирует.
    flexGrow = KU and 1 or nil,
    onChange = function(v)
      volDraft = math.max(0, math.min(100, math.floor(tonumber(v) or 0)))
    end,
    onDragEnd = function()
      local final = volDraft or current
      volDraft = nil
      poke("vol-set", tostring(math.max(0, math.min(100, math.floor(tonumber(final) or 0)))))
    end,
  }
  -- Kumihimo: тёплая палитра слайдера из поддерживаемых реконсилютором
  -- свойств (fill/track/thumb/focusColor) — иначе хост-синий ползунок.
  if KU then
    local c = KT.colors
    sliderProps.fill = c.accent
    sliderProps.track = c.outline
    sliderProps.thumb = c.text
    sliderProps.focusColor = c.accent
  end
  return ui.row({
    key = "np-volume",
    gap = 6,
    align = "center",
    -- Kumihimo: строка громкости ФИКСИРОВАННОЙ ширины (глиф 14 + зазор 6
    -- + слайдер 96), а слайдер внутри растёт. Хост НЕ применяет width к
    -- ui.slider — реконсилятор его не читает, и слайдер мерится дефолтными
    -- 180, ломая бюджет правой зоны транспорта; точную ширину получает
    -- только grow-ребёнок. Вне Kumihimo строка остаётся натуральной.
    width = KU and GEO.npVolW or nil,
  }, {
    ui.row({
      key = "np-volume-focus", width = 20, align = "center",
      onClick = function()
        volumeKeys = true
        typing = false
        insertPending = nil
        endHold()
        draw()
      end,
    }, {
      ui.glyph({ key = "np-volume-glyph", name = volumeGlyph(current), size = 14, color = COLORS.muted }),
    }),
    ui.slider(sliderProps),
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
    local loopMode = tostring(s.loop or "none")
    -- Кнопки транспорта создаются ОДИН раз: обработчики, глифы и режимы у
    -- обеих семейств одни и те же — различаются только раскладка
    -- (Kumihimo собирает из них три зоны, см. ниже) и палитра
    -- (KUF.warmButton / KUF.warmToggleProps вне Kumihimo возвращают props
    -- нетронутыми, поэтому вид non-KU не меняется).
    local btnPrev = ui.button(KUF.warmButton({
      key = "prev",
      glyph = "player-skip-back",
      controlSize = "sm",
      variant = "ghost",
      onClick = function()
        poke("prev")
      end,
    }))
    -- Toggle — тёплый очерченный круг 32/radius16 (не сплошная плашка);
    -- вне Kumihimo props не тронуты и работает прежний variant=primary.
    local btnToggle = ui.button(KUF.warmToggleProps({
      key = "toggle",
      glyph = s.status == "playing" and "player-pause" or "player-play",
      controlSize = "sm",
      variant = "primary",
      onClick = function()
        poke("toggle")
      end,
    }))
    local btnNext = ui.button(KUF.warmButton({
      key = "next",
      glyph = "player-skip-forward",
      controlSize = "sm",
      variant = "ghost",
      onClick = function()
        poke("next")
      end,
    }))
    local btnShuffle = ui.button(KUF.warmButton({
      key = "shuffle",
      glyph = "arrows-shuffle",
      controlSize = "sm",
      variant = s.shuffle and "primary" or "ghost",
      onClick = function()
        poke("shuffle", "toggle")
      end,
    }, s.shuffle == true))
    -- Три различимых состояния повтора: track — «repeat-once», queue —
    -- «repeat», оба primary; none — тусклый ghost.
    local btnRepeat = ui.button(KUF.warmButton({
      key = "repeat",
      hintIdentity = "repeat:" .. loopMode,
      glyph = loopMode == "track" and "repeat-once" or "repeat",
      controlSize = "sm",
      variant = loopMode ~= "none" and "primary" or "ghost",
      onClick = function()
        poke("repeat", REPEAT_CYCLE[loopMode] or "none")
      end,
    }, loopMode ~= "none"))
    local trackId = tostring(s.trackId or "")
    local caps = capsOfProvider(s.provider)
    local rating = ratingOf(s, trackId)
    local btnLike = (trackId ~= "" and type(caps) == "table" and caps.rate == true)
      and ui.button(KUF.warmButton({
        key = "np-like", glyph = rating == "liked" and "heart-filled" or "heart",
        controlSize = "sm", variant = rating == "liked" and "primary" or "ghost",
        hintIdentity = trackId .. ":" .. tostring(rating),
        onClick = function()
          poke("rate", (rating == "liked" and "none:" or "like:") .. trackId)
        end,
      }, rating == "liked")) or nil
    local volume = volumeNode(s)
    -- Подпись режима повтора для Kumihimo — ОБЫЧНЫЙ lbl: ui.label берёт
    -- ОДНУ таблицу props, поэтому прежний вызов ui.label(text, props)
    -- оставлял подпись пустой. Бюджет строки транспорта посчитан на одну
    -- строку, отсюда maxLines=1. В non-KU ветке (ниже) тот прежний вызов
    -- сохранён как есть — её рендер менять запрещено.
    local repeatShort = (loopMode ~= "none") and lbl(REPEAT_SHORT[loopMode] or loopMode, {
      key = "np-repeat-short",
      size = 11,
      color = COLORS.muted,
      mono = true,
      maxLines = 1,
    }) or nil
    -- Ошибка важнее меток (она объясняет срыв), метки — фолбэк.
    local err = tostring(s.lastError or "")
    -- Kumihimo: колонка текста получает РЕАЛЬНУЮ ширину из GEO, а подписи —
    -- maxLines=1: иначе натуральная длина 48-символьного заголовка
    -- распирала бы строку транспорта. Вне KU ширина по-прежнему от
    -- раскладки, а maxLines не выставляется.
    local textColumn = ui.column({ key = "np-text", gap = 2, flexGrow = 1, width = KU and GEO.npTextW or nil }, compact(
      searchLabel(providerGlyph(s.provider) .. shorten(tostring(s.title or ""), 48), {
        key = "np-title",
        size = 15,
        color = COLORS.text,
        maxLines = KU and 1 or nil,
        maxWidth = KU and GEO.npTextW or nil,
      }, s.title, "playing-title:" .. tostring(s.trackId)),
      searchLabel(shorten(tostring(s.artist or ""), 48), {
        key = "np-artist",
        size = 12,
        color = COLORS.muted,
        maxLines = KU and 1 or nil,
        maxWidth = KU and GEO.npTextW or nil,
      }, s.artist, "playing-artist:" .. tostring(s.trackId)),
      -- Только дефолт держит причину отдельной строкой текста: у
      -- Kumihimo она переезжает в правую колонку под время (ниже),
      -- чтобы высота арт-строки осталась 48 и бюджет транспорта — 110.
      (not KU) and err ~= "" and lbl(shorten("! " .. err, 56), { key = "np-error", size = 11, color = COLORS.error }) or nil,
      (not KU) and lbl(#marks > 0 and table.concat(marks, " · ") or "", {
        key = "np-marks",
        size = 11,
        color = COLORS.muted,
      }) or nil
    ))
    local timeLabel = lbl(clock(pos) .. (dur > 0 and (" / " .. clock(dur)) or ""), {
      key = "np-time",
      size = 12,
      color = COLORS.muted,
      mono = true,
      -- Kumihimo: время живёт в правой колонке своего бюджета (GEO.npTimeW).
      maxLines = KU and 1 or nil,
      maxWidth = KU and GEO.npTimeW or nil,
    })
    if KU then
      -- Транспорт Kumihimo — три зоны по факту, без «оба края grow»:
      -- слева режимы (shuffle/repeat) и now-playing, в центре
      -- prev/toggle/next, справа громкость и время со статусом. Ширины зон
      -- посчитаны в recomputeLayout от реальной ширины карточки (GEO.np*),
      -- поэтому длинный заголовок или статус не вытесняет контролы за
      -- край. Все прежние действия, глифы и метки сохранены 1:1.
      local statusText = err ~= "" and ("! " .. shorten(err, 48))
        or (#marks > 0 and table.concat(marks, " · ") or "")
      table.insert(children, KUF.npTopRow({
        leftW = GEO.npLeftW,
        rightW = GEO.npRightW,
        left = compact(
          artNode(s.artPath, 48),
          textColumn,
          ui.row({ key = "np-modes", gap = 6, align = "center" }, compact(btnLike, btnShuffle, btnRepeat, repeatShort))
        ),
        center = compact(btnPrev, btnToggle, btnNext),
        right = compact(
          volume,
          ui.column({ key = "np-time-col", gap = 2, align = "end", width = GEO.npTimeW }, compact(
            timeLabel,
            statusText ~= "" and lbl(statusText, {
              key = "np-status",
              size = 11,
              color = err ~= "" and COLORS.error or COLORS.muted,
              maxLines = 1,
              maxWidth = GEO.npTimeW,
            }) or nil
          ))
        ),
      }))
    else
      -- Non-KU транспорт — ровно прежний состав и порядок: арт, текст,
      -- время и ряд контролов (prev/toggle/next/shuffle/repeat + подпись
      -- режима + громкость). Прежний вызов ui.label(text, props) для
      -- подписи режима оставлен как есть: рендер non-KU не меняем.
      local controls = { btnPrev, btnToggle, btnNext, btnShuffle, btnRepeat }
      if btnLike then table.insert(controls, btnLike) end
      if loopMode ~= "none" then
        table.insert(controls, ui.label(REPEAT_SHORT[loopMode] or loopMode, {
          key = "np-repeat-short",
          size = 11,
          color = COLORS.muted,
          fontFamily = skin.MONO,
        }))
      end
      table.insert(controls, volume)
      table.insert(children, ui.row({ key = "np-top", gap = 12, align = "center" }, {
        artNode(s.artPath, 48),
        textColumn,
        timeLabel,
        ui.row({ key = "np-controls", gap = 6, align = "center" }, controls),
      }))
    end
    if dur > 0 then
      local progressProps = {
        key = "np-progress",
        progress = math.max(0, math.min(1, pos / dur)),
        height = 4,
        radius = 2,
      }
      -- Kumihimo: полоса НЕ растёт по оси колонки (той же оси, что и
      -- карточка) — только фиксированная высота 4, ширину даёт stretch
      -- карточки. Вне Kumihimo — прежний flexGrow=1.
      if not KU then
        progressProps.flexGrow = 1
      end
      -- Тёплая полоса прогресса из поддерживаемых свойств (fill/track):
      -- иначе дефолтная хост-синяя заливка.
      if KU then
        progressProps.fill = KT.colors.accent
        progressProps.track = KT.colors.outline
      end
      table.insert(children, ui.progress(progressProps))
    end
  else
    -- Без трека геометрия та же: карточка с текстом вместо кнопок —
    -- соседние вью не прыгают при старте/остановке демона.
    local stateTitle = lbl(s.linked and "Nothing playing" or "tmusd is not responding", {
      key = "np-state-title",
      size = 15,
      color = COLORS.text,
      maxLines = KU and 1 or nil,
      maxWidth = KU and GEO.npTextW or nil,
    })
    -- Подсказка — только без связи; ошибка видна и без трека. Бюджет
    -- правой зоны Kumihimo (GEO.npStateW) держит обе подписи одной
    -- строкой: длинная ошибка эллипсится, а не вылезает за карточку.
    local hintLabel = (not s.linked) and lbl("start it with: systemctl --user start tmusd", {
      key = "np-state-hint",
      size = 12,
      color = COLORS.muted,
      maxLines = KU and 1 or nil,
      maxWidth = KU and GEO.npStateW or nil,
    }) or nil
    local errLabel = (tostring(s.lastError or "") ~= "") and lbl(shorten("! " .. tostring(s.lastError), 56), {
      key = "np-error",
      size = 11,
      color = COLORS.error,
      maxLines = KU and 1 or nil,
      maxWidth = KU and GEO.npStateW or nil,
    }) or nil
    if KU then
      -- Kumihimo: подсказка/ошибка — в правой колонке (как метки
      -- транспорта), левая строка остаётся высотой 48; иначе три строки
      -- (54 px) раздули бы явно ограниченную карточку. Обе зоны имеют
      -- реальную ширину из GEO (центр отсутствует — правая вбирает его).
      table.insert(children, KUF.npTopRow({
        leftW = GEO.npLeftW,
        rightW = GEO.npStateW,
        left = {
          ui.glyph({
            key = "np-state-glyph",
            name = s.linked and "music" or "plug-off",
            color = COLORS.muted,
          }),
          ui.column({ key = "np-text", gap = 2, flexGrow = 1 }, { stateTitle }),
        },
        right = {
          ui.column({ key = "np-time-col", gap = 2, align = "end", width = GEO.npStateW }, compact(hintLabel, errLabel)),
        },
      }))
    else
      table.insert(children, ui.row({ key = "np-top", gap = 12, align = "center" }, {
        ui.glyph({
          key = "np-state-glyph",
          name = s.linked and "music" or "plug-off",
          color = COLORS.muted,
        }),
        ui.column({ key = "np-text", gap = 2, flexGrow = 1 }, compact(
          stateTitle,
          hintLabel,
          errLabel
        )),
      }))
    end
  end
  if KU then
    -- Kumihimo: ровно реальные 24 полосы хоста (публикует виджет),
    -- каждая grow1 в полноширинном ряду — никакой короткой левой ленты.
    -- Нет CSV или пауза — пустой СТАБИЛЬНЫЙ слот 18 px (высота карточки не
    -- прыгает), а не 56 выдуманных баров.
    local live = spectrum ~= nil and s.status == "playing"
    table.insert(children, ui.row({ key = "np-spec", height = 18, align = "end" },
      compact(live and KUF.kuSpectrum(spectrum) or nil)))
  else
    -- Полоса спектра ВСЕГДА присутствует: без живого спектра или на
    -- паузе — «спящая» базовая линия 1 px цветом border, чтобы высота
    -- карточки не прыгала на каждом кадре.
    local specColor = (spectrum and s.status == "playing") and COLORS.accent or COLORS.border
    table.insert(children, ui.row({ key = "np-spec", height = 18, align = "center" }, {
      spectrumBars(NP_SPEC_BARS, 14, 5, 2, specColor),
    }))
  end
  -- Высота карточки Kumihimo фиксирована бюджетом: 48 (np-top) + 8 + 4
  -- (прогресс) + 8 + 18 (спектр) + 2×12 паддинга = 110 = GEO.npcH. Явная
  -- высота держит карточку ровно в бюджете даже без прогресса.
  return card(children, {
    key = "np-block",
    fill = COLORS.npbg,
    gap = 8,
    padding = 12,
    height = KU and GEO.npcH or nil,
  })
end

local function trackRow(i, t, opts)
  local o = opts or {}
  local active = i == cursor
  local current = o.current
  -- Маркер рейтинга перед названием: ♥ лайк, × дизлайк; нет — ничего.
  local marker = o.rating == "liked" and "♥ " or (o.rating == "disliked" and "× " or "")
  local onClick = function()
    cursor = i
    selected = o
    -- Клик по треку поиска — тот же контекстный запуск, что и Enter
    -- (activate), иначе клик и клавиша играют по-разному. Решение и
    -- данные берём из самой строки rows() (t): в opts нет ни kind,
    -- ни id, поэтому прежняя проверка o.kind/o.id не срабатывала и
    -- клик уходил одиночным радио мимо контекста.
    activate(t, i)
    draw()
  end
  -- Kumihimo: компактная ОФОРМЛЕННАЯ строка выдачи (44): индекс 22, арт 28,
  -- вертикальная колонка заголовок-подзаголовок (grow1, maxLines=1) и —
  -- когда длительность есть — ограниченный слот 44. Тот же хелпер стилизует
  -- и строки плейлиста, не меняя их логику. Курсор (рамка accent + заливка
  -- selected) живёт ТОЛЬКО при фокусе буфера 2; реально играющий трек
  -- независимо получает маркер ▶ и акцент — выдуманного playing нет.
  if KU then
    local playing = t ~= nil and t.id ~= nil and t.id ~= ""
      and t.id == tostring(snapshot.trackId or "")
    local focused = (i == cursor) and focusBuf == 2
    local time = o.time
    return ui.row({
      key = "track" .. i,
      hintIdentity = view .. ":" .. playlistId .. ":" .. i .. ":" .. tostring(t.id),
      gap = 10,
      paddingH = 10,
      paddingV = 6,
      height = TRACK_ROW_HEIGHT,
      radius = 6,
      shape = "chamfer",
      border = focused and COLORS.accent or KT.colors.outline,
      borderWidth = 1,
      fill = focused and COLORS.selected or KT.colors.surfaceVariant,
      align = "center",
      onClick = onClick,
    }, compact(
      ui.row({ key = "kmark" .. i, width = 22, align = "center" }, {
        lbl(playing and "▶" or tostring(i), {
          key = "kidx" .. i,
          size = 12,
          color = playing and COLORS.accent or COLORS.muted,
          mono = true,
          maxLines = 1,
        }),
      }),
      artNode(o.art, 28),
      -- Бейдж провайдера и рейтинг-маркер остаются в тексте заголовка;
      -- обрезка по реальной ширине колонки, без счётчика символов.
      ui.column({ key = "ktext" .. i, gap = 2, flexGrow = 1 }, {
        searchLabel(providerGlyph(providerByTrack(t)) .. marker .. tostring(o.title or ""), {
          key = "ktitle" .. i,
          size = 13,
          color = playing and COLORS.accent or COLORS.text,
          maxLines = 1,
        }, (t.kind == "track" or t.kind == "artist") and t.rawTitle or "",
          "row-title:" .. tostring(t.id)),
        searchLabel(tostring(o.subtitle or ""), {
          key = "ksub" .. i,
          size = 11,
          color = COLORS.muted,
          maxLines = 1,
        }, t.artist, "row-artist:" .. tostring(t.id)),
      }),
      (time ~= nil and time ~= "") and ui.row({
        key = "ktime-w" .. i,
        width = 44,
        justify = "end",
        align = "center",
      }, {
        lbl(time, { key = "ktime" .. i, size = 11, color = COLORS.muted, mono = true, maxLines = 1 }),
      }) or nil
    ))
  end
  local children = {
    ui.row({ key = "mark" .. i, width = 28, align = "center" }, {
      lbl(current and "▶" or (o.index or tostring(i)), {
        key = "idx" .. i,
        color = current and COLORS.accent or COLORS.muted,
        mono = true,
      }),
    }),
    artNode(o.art, 32),
    -- Бейдж провайдера перед названием (и рейтинг-маркером).
    searchLabel(shorten(providerGlyph(providerByTrack(t)) .. marker .. o.title, 52), {
      key = "title" .. i,
      grow = 1,
      color = current and COLORS.accent or (active and COLORS.text or COLORS.muted),
    }, (t.kind == "track" or t.kind == "artist") and t.rawTitle or "",
      "row-title:" .. tostring(t.id)),
    searchLabel(shorten(o.subtitle, 32), { key = "sub" .. i, color = COLORS.muted, maxWidth = 220 },
      t.artist, "row-artist:" .. tostring(t.id)),
    lbl(o.time or "", { key = "time" .. i, color = COLORS.muted, width = 56, mono = true }),
  }
  return ui.row({
    key = "track" .. i,
    hintIdentity = view .. ":" .. playlistId .. ":" .. i .. ":" .. tostring(t.id),
    gap = 10,
    paddingH = 10,
    paddingV = 6,
    height = TRACK_ROW_HEIGHT,
    radius = 6,
    shape = SC and "chamfer" or nil,
    align = "center",
    -- Играющий трек важнее курсора: у них разные заливки, иначе
    -- «что сейчас играет» не отличить от «где курсор» при скролле.
    fill = current and COLORS.playing or (active and COLORS.selected or COLORS.bg),
    onClick = onClick,
  }, children)
end

local function resultRow(r)
  local kind = tostring(r.kind or "")
  local artists = type(r.artists) == "table" and table.concat(r.artists, ", ") or ""
  local row = {
    kind = kind,
    id = idOf(r),
    title = tostring(r.title or r.name or ""),
    rawTitle = tostring(r.title or r.name or ""),
    artist = artists,
    subtitle = artists ~= "" and artists or tostring(r.album or ""),
    art = tostring(r.art_path or ""),
  }
  if kind == "track" then
    row.rating = ratingOf(snapshot, row.id)
    -- event/payload — одиночный фолбэк для activate: основной путь —
    -- контекст всей видимой выдачи через play-context (contextPayload).
    row.event = "radio"
    row.payload = idOf(r)
  elseif kind == "playlist" then
    -- Активация плейлиста из выдачи — PlayPlaylist без --track
    -- (старт с заголовка плейлиста).
    row.event = "play-playlist"
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
            -- Абсолютная позиция в плейлисте (0-базная, по data.tracks,
            -- а не по отфильтрованному выводу): локальное удаление
            -- (pl rm-at) бьёт по позиции — дубликаты уходят по одному.
            pos = i - 1,
            rating = ratingOf(s, idstr),
            title = tostring(t.title or ""),
            rawTitle = tostring(t.title or ""),
            artist = type(t.artists) == "table" and table.concat(t.artists, ", ") or "",
            subtitle = type(t.artists) == "table" and #t.artists > 0
              and table.concat(t.artists, ", ") or tostring(t.album or ""),
            art = tostring(t.art_path or ""),
            event = "play-playlist",
            payload = playlistId .. "|" .. idstr,
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
  if view == "library" then
    return CARD_WINDOW
  elseif view == "search" then
    -- У поиска над списком ещё строка ввода (34) и ряд чипов (28) со
    -- своими зазорами — окно меньше, чем у прочих списков. Считается
    -- здесь же: отдельное имя на весь чанк не заводим (лимит локалов).
    -- У Kumihimo эта оценка недостижима: clampCursor для KU Search
    -- возвращается раньше, а draw зовёт авторитетный KUF.searchSlice.
    return math.floor((GEO.viewH - (34 + 28 + 2 * GAP) + TRACK_ROW_GAP)
      / (TRACK_ROW_HEIGHT + TRACK_ROW_GAP))
  elseif view == "clients" and KU then
    -- Реальный ряд Kumihimo (64) и зазор 6; над списком заголовок ~24 и
    -- зазор GAP. Минимум одна строка — окно никогда не ноль.
    return math.max(1, math.floor((GEO.viewH - (24 + GAP) + GEO.clientGap)
      / (GEO.clientRow + GEO.clientGap)))
  end
  return LIST_WINDOW
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
  -- Kumihimo Search: окно считает KUF.searchSlice по реальным заголовкам
  -- групп — count-базовая арифметика тут не годится.
  if KU and view == "search" then
    return
  end
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

-- Kumihimo Search: ОДНА ограниченная арифметика видимого окна. Каждый
-- отрисованный результат стоит TRACK_ROW_HEIGHT (44), каждый заголовок
-- группы — 18, а между ЛЮБЫМИ соседними узлами списка лежит зазор 6.
-- Курсор всегда остаётся в подогнанном срезе; окно сдвигается ровно
-- настолько, насколько нужно (курсор у нижнего края — вверх добираем
-- бюджетом), как clampScroll. Обход ограничен бюджетом вьюпорта, поэтому
-- 8-Гц перерисовка спектра не сканирует всю выдачу: ходим только рядом с
-- текущим окном/курсором. Возвращает (first, slice) — как visibleWindow.
function KUF.searchSlice(items, budget)
  local n = #items
  if n == 0 then
    cursor, windowTop = 1, 1
    return 1, {}
  end
  local c = cursor
  if c < 1 then
    c = 1
  elseif c > n then
    c = n
  end
  cursor = c
  local rowH, headH, nodeGap = TRACK_ROW_HEIGHT, 18, 6
  -- Высота среза [top..last] с заголовком у первого узла и на каждой
  -- смене вида. Возвращает последний влезающий индекс (не меньше top).
  local function sliceEnd(top)
    local r, h, last = 0, 0, top - 1
    for i = top, n do
      local hdr = (i == top or items[i].kind ~= items[i - 1].kind) and 1 or 0
      local nr, nh = r + 1, h + hdr
      if rowH * nr + headH * nh + nodeGap * (nr + nh - 1) > budget then
        break
      end
      r, h, last = nr, nh, i
    end
    if last < top then
      last = top
    end
    return last
  end
  local top = windowTop
  if top < 1 then
    top = 1
  elseif top > n then
    top = n
  end
  if top > c then
    top = c
  end
  local last = sliceEnd(top)
  if last < c then
    -- Курсор ушёл ниже окна: сдвигаем верх минимально — заполняем
    -- бюджет ВВЕРХ от курсора (курсор становится последним видимым).
    local i, r, h = c, 1, 1
    while i > 1 do
      local hdr = (items[i - 1].kind ~= items[i].kind) and 1 or 0
      local nr, nh = r + 1, h + hdr
      if rowH * nr + headH * nh + nodeGap * (nr + nh - 1) > budget then
        break
      end
      r, h, i = nr, nh, i - 1
    end
    top = i
    last = sliceEnd(top)
  end
  windowTop = top
  local out = {}
  for i = top, last do
    out[#out + 1] = items[i]
  end
  return top, out
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
  maybeHomeMore()
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

-- Активация карточки home: трек — радио (`tmus radio <prov:id>`): демон
-- сам строит контекст рекомендациями. Контекстные жесты (плейлист,
-- альбом) сохраняют прежнюю семантику: плейлист открывается/играется
-- целиком (PlayPlaylist), его треки — с --track. Треки выдачи поиска
-- контекстны и идут не сюда, а через activate (см. contextPayload).
local function homeActivate()
  local shelf = homeShelves()[homeShelf]
  local c = shelf and shelf.cards[homeItem]
  if c == nil then
    return
  end
  if c.kind == "playlist" then
    openPlaylist(c)
  elseif c.kind == "track" then
    poke("radio", c.id)
  else
    poke(c.event or "radio", c.payload)
  end
end

-- Payload контекстного запуска "<start>|<id1>,<id2>,...": треки из
-- items (порядок отображения) без пустых id, start — 0-базный номер
-- активной строки среди этих треков. Активная строка не трек, треков
-- нет или строка вне items — nil, вызывающий падает назад на одиночный
-- запуск строки.
local function contextPayload(activePos, items)
  local ids, start = {}, nil
  for pos, it in ipairs(items) do
    if it.kind == "track" and it.id ~= nil and it.id ~= "" then
      if pos == activePos then
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

-- Активация строки списка клавишей или кликом. `pos` — номер строки в
-- rows() (для Enter — cursor, для клика — индекс нажатой строки): по
-- нему считается контекстный старт поиска.
function activate(row, pos)
  if row == nil then
    return
  end
  selected = row
  if row.kind == "eq-preset" then
    -- Имя берём из живого snapshot: между рендером строки и нажатием
    -- пресет мог смениться (h/l листает соседний). preset-on, а не
    -- preset: выбор пресета включает эквалайзер, иначе «выбрал
    -- пресет, а он всё ещё Off».
    local eq = snapshot.equalizer
    if type(eq) == "table" then
      poke("eq", "preset-on:" .. tostring(eq.preset or ""))
    end
    return
  end
  if row.open then
    openPlaylist(row)
  elseif row.event == "radio" then
    -- Трек-строка поиска (event "radio" ставят только треки выдачи):
    -- играем контекст всей видимой выдачи в порядке rows(), старт — с
    -- активированной строки. Старт считается по номеру строки (pos), а
    -- не по совпадению id: на дублях в выдаче поиск по id взял бы
    -- первое вхождение и запустил бы не ту строку. Контекст не собрался
    -- (нет позиции, строка вне rows()) — одиночное радио строки.
    local payload = view == "search" and pos ~= nil and contextPayload(pos, rows()) or nil
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

-- Догрузка home-ленты: курсор полок подошёл к последней загруженной —
-- просим сервис следующую страницу по курсору продолжения. Слои
-- защиты те же, что у плейлиста: in-flight guard сервиса, исчерпание
-- (next == nil после дочитки) и локальный дебаунс 500 мс.
local HOME_MORE_MARGIN = 1
local lastHomeMoreAt = 0
maybeHomeMore = function()
  if view ~= "home" then
    return
  end
  local feed = homeFeed
  if type(feed) ~= "table" or feed.status ~= "ok" then
    return
  end
  local nextCursor = tostring(feed.next or "")
  if nextCursor == "" then
    return
  end
  local shelves = homeShelves()
  if homeShelf < #shelves - HOME_MORE_MARGIN then
    return
  end
  local now = noctalia.nowMs()
  if now - lastHomeMoreAt < MORE_DEBOUNCE_MS then
    return
  end
  lastHomeMoreAt = now
  poke("home-more", nextCursor)
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
  hints:cancel()
  volumeKeys = false
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
  local sc = SC
  for i, v in ipairs(VIEWS) do
    local active = view == v.id or (view == "playlist" and v.id == "library")
    table.insert(items, ui.column({
      key = "rail-" .. v.id,
      width = 48,
      gap = 2,
      align = "center",
      paddingV = 8,
      radius = 8,
      shape = sc and "chamfer" or nil,
      fill = active and COLORS.selected or COLORS.surface,
      -- Клик по пункту сайдбара — та же смена вида, что ctrl+j/k и
      -- цифры: путь «клик по рейлу» упоминался в комментариях
      -- setView, но обработчика у пункта не было.
      onClick = function()
        setView(v.id)
        draw()
      end,
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
    fill = COLORS.chrome,
    radius = sc and CHAM or 12,
    shape = sc and "chamfer" or nil,
    border = sc and COLORS.border or nil,
    borderWidth = sc and 1 or nil,
    align = "center",
  }, items)
end

-- Кумихимо: пять горизонтальных подвесных вкладок вместо левого рейла.
-- Источник — тот же VIEWS; активная вкладка — плюс Library для вложенного
-- playlist (как в рейле). Каждая вкладка — shared-ярлык skin.kumihimoTag:
-- подвес декоративен, цель нажатия — тело. Подпись и глиф — как в рейле.
function KUF.tabWidth()
  return math.max(72, math.floor((GEO.bodyW - (#VIEWS - 1) * GAP) / #VIEWS))
end

function KUF.tabs()
  local tabs = {}
  local w = KUF.tabWidth()
  for _, v in ipairs(VIEWS) do
    local active = view == v.id or (view == "playlist" and v.id == "library")
    table.insert(tabs, skin.kumihimoTag({
      key = "ku-tab-" .. v.id,
      width = w,
      height = GEO.header,
      gap = 4,
      -- Паддинг 7 — минимум против выступа контента за chamfer-срез.
      padding = 7,
      active = active,
      onClick = function()
        setView(v.id)
        draw()
      end,
    }, {
      ui.row({ key = "ku-tab-in-" .. v.id, gap = 8, align = "center" }, {
        ui.glyph({
          key = "ku-tab-glyph-" .. v.id,
          name = v.glyph,
          size = 15,
          color = active and KT.colors.accent or KT.colors.muted,
        }),
        lbl(v.title, {
          key = "ku-tab-label-" .. v.id,
          size = 12,
          color = active and KT.colors.text or KT.colors.muted,
          -- Тело вкладки — слот 20 px на одну строку: без maxLines=1
          -- длинное имя вида («Equalizer») переносилось бы и выступало
          -- за chamfer-срез.
          maxLines = 1,
        }),
      }),
    }))
  end
  return ui.row({ key = "ku-tabs", gap = GAP, align = "start" }, tabs)
end

-- Шапка панели: знак скина, имя плагина и активный вид слева; краткая
-- сводка играющего и состояние связи справа. Сводка ТОЛЬКО читающая:
-- кнопки, прогресс и спектр живут в транспортной карточке (npNode)
-- внизу workspace — дублировать управление нельзя, разъедется состояние.
local function headerNode(s)
  local current = VIEWS[VIEW_IDS[view] or 0]
  local title = (view == "playlist" and playlistTitle ~= "") and playlistTitle
    or (current and current.title or tostring(view))
  local hasTrack = s.linked and (s.title or "") ~= ""
  local summary
  if hasTrack then
    local artists = tostring(s.artist or "")
    summary = artists ~= "" and (artists .. " — " .. tostring(s.title or "")) or tostring(s.title or "")
  elseif s.linked then
    summary = "Nothing playing"
  else
    summary = "tmusd is not responding"
  end
  local stateText
  if not s.linked then
    stateText = "нет связи"
  elseif hasTrack then
    stateText = s.status == "playing" and "играет" or (s.status == "paused" and "пауза" or "стоп")
  else
    stateText = "простой"
  end
  local sc = SC
  -- Активная Стейджикрафт-семья не показывает blossom-эмблему: знак
  -- рисуется родным глифом в акценте. В остальных случаях — прежняя
  -- картинка эмблемы.
  local brand = sc
    and ui.glyph({ key = "brand-glyph", name = "music", size = 16, color = COLORS.accent })
    or ui.image({
      key = "brand-mark",
      path = "skin-assets/emblem.png",
      width = 18,
      height = 18,
      radius = 9,
      fit = "cover",
    })
  return ui.row({
    key = "header",
    height = GEO.header,
    gap = 10,
    paddingH = PAD,
    radius = sc and CHAM or (GEO.header // 2),
    shape = sc and "chamfer" or nil,
    align = "center",
    fill = COLORS.chrome,
    border = sc and COLORS.border or nil,
    borderWidth = sc and 1 or nil,
  }, {
    brand,
    lbl("Музыка", { key = "brand-title", size = 13, color = COLORS.text }),
    lbl(shorten(title, 24), { key = "header-view", size = 11, color = COLORS.muted, maxWidth = 240 }),
    lbl("", { key = "header-spacer", grow = 1 }),
    lbl(shorten(summary, 56), {
      key = "header-np",
      size = 11,
      color = hasTrack and COLORS.accent or COLORS.muted,
      maxWidth = 420,
    }),
    lbl(stateText, { key = "header-state", size = 10, color = COLORS.muted }),
    lbl(#(s.queue or {}) .. " в очереди", {
      key = "header-queue",
      size = 10,
      color = COLORS.muted,
      mono = true,
    }),
  })
end

local function stateNode(s, items)
  if not s.linked then
    return card(compact(
      ui.glyph({ key = "st-glyph", name = "plug-off", size = KU and 16 or nil, color = COLORS.muted }),
      lbl("tmusd is not responding", {
        key = "st-title",
        size = KU and 13 or nil,
        color = COLORS.text,
        maxLines = KU and 1 or nil,
      }),
      lbl("start it with: systemctl --user start tmusd", {
        key = "st-hint",
        size = KU and 11 or nil,
        color = COLORS.muted,
        maxLines = KU and 1 or nil,
      })
    ), { key = "state-card", fill = COLORS.surface, shape = KU and "chamfer" or nil })
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
  return card(compact(
    ui.glyph({
      key = "st-glyph",
      name = (searching or (view == "playlist" and tostring(s.tracksId or "") ~= playlistId)) and "loader" or "music",
      size = KU and 16 or nil,
      color = COLORS.muted,
    }),
    lbl(text, {
      key = "st-title",
      size = KU and 13 or nil,
      color = COLORS.muted,
      maxLines = KU and 1 or nil,
    })
  ), { key = "state-card", shape = KU and "chamfer" or nil })
end

local function contextTitle(text)
  return lbl(text, { key = "ctx-title", size = 12, color = COLORS.muted })
end

local function listChildren(slice, first)
  local children = {}
  local lastKind = nil
  -- Kumihimo всегда рисует заголовок у первого узла среза (в бюджет окна
  -- это заложено); при не-KU сохраняется прежняя логика «нет заголовка,
  -- если вид не сменился».
  if view == "search" and not KU and first > 1 then
    local prior = rows()[first - 1]
    lastKind = prior and prior.kind or nil
  end
  for k, row in ipairs(slice) do
    local i = first + k - 1
    if view == "search" and row.kind ~= lastKind then
      if KU then
        -- Тихий акцентный штрих + заголовок группы (11, one-line).
        -- Орнаментов на каждый трек нет — только на границе вида.
        -- Ключи включают абсолютный индекс строки i: если упорядоченная
        -- выдача возвращается к уже встречавшемуся виду, kind-ключ дал бы
        -- дубликат-сиблинг. Не-KU конвенцию ключей не трогаем.
        table.insert(children, ui.row({
          key = "group-" .. i,
          height = 18,
          gap = 8,
          align = "center",
        }, {
          ui.box({ key = "group-tick-" .. i, width = 2, height = 12, radius = 1, fill = COLORS.accent }),
          lbl(GROUP_TITLES[row.kind] or row.kind, {
            key = "group-l-" .. i,
            size = 11,
            color = COLORS.muted,
            maxLines = 1,
          }),
        }))
      else
        table.insert(children, lbl(GROUP_TITLES[row.kind] or row.kind, {
          key = "group-" .. row.kind,
          size = 12,
          color = COLORS.muted,
        }))
      end
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
      if KU then
        -- Курсор и выбранный источник независимы: источник помечается
        -- ТОЛЬКО глифом circle-check/accent, курсор (фокус буфера 2) —
        -- рамкой accent + заливкой selected. Выбранный источник не
        -- «разогревается» под клавиатурный фокус. Restart — нейтральное
        -- действие с отдельным глифом refresh.
        local focused = (i == cursor) and focusBuf == 2
        table.insert(children, ui.row({
          key = "client" .. i,
          hintIdentity = tostring(row.event) .. ":" .. tostring(row.payload),
          gap = 10,
          paddingH = 12,
          paddingV = 8,
          height = GEO.clientRow,
          radius = 8,
          shape = "chamfer",
          border = focused and COLORS.accent or KT.colors.outline,
          borderWidth = 1,
          fill = focused and COLORS.selected or KT.colors.surfaceVariant,
          align = "center",
          onClick = function()
            cursor = i
            activate(row, i)
            draw()
          end,
        }, {
          ui.glyph({
            key = "client-glyph" .. i,
            name = row.active and "circle-check" or (row.restart and "refresh" or "server"),
            size = 16,
            color = row.active and COLORS.accent or COLORS.muted,
          }),
          -- Центральная колонка растёт и несёт title/subtitle одной
          -- строкой: длинная подсказка не выдавливает статус за край.
          ui.column({ key = "client-text" .. i, gap = 2, flexGrow = 1 }, {
            lbl(glyph ~= "" and (glyph .. row.title) or row.title, {
              key = "client-title" .. i,
              size = 13,
              color = COLORS.text,
              maxLines = 1,
            }),
            lbl(hint ~= "" and hint or row.subtitle, {
              key = "client-sub" .. i,
              size = 11,
              color = COLORS.muted,
              maxLines = 1,
            }),
          }),
          ui.row({ key = "client-status-w" .. i, width = 104, justify = "end", align = "center" }, {
            lbl(stateText, {
              key = "client-state" .. i,
              size = 11,
              color = stateColor,
              maxLines = 1,
            }),
          }),
        }))
      else
        table.insert(children, ui.row({
          key = "client" .. i,
          hintIdentity = tostring(row.event) .. ":" .. tostring(row.payload),
          gap = 10,
          padding = 12,
          height = TRACK_ROW_HEIGHT,
          radius = 6,
          shape = SC and "chamfer" or nil,
          align = "center",
          fill = (i == cursor or row.active) and COLORS.selected or COLORS.surface,
          onClick = function()
            cursor = i
            activate(row, i)
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
      end
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
      searchLabel(providerGlyph(s.provider) .. shorten(tostring(s.title or ""), 40),
        { key = "np-title", size = 18, color = COLORS.text }, s.title, "hero-title:" .. tostring(s.trackId)),
      searchLabel(shorten(tostring(s.artist or ""), 40),
        { key = "np-artist", size = 13, color = COLORS.accent }, s.artist, "hero-artist:" .. tostring(s.trackId)),
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
  local badge = lbl("", { key = "hf-action-hint", size = 10, mono = true })
  local identity = "home:" .. shelfIndex .. ":" .. cardIndex .. ":" .. c.id
  -- Kumihimo: карточка подвешена на шнуре полки общим ярлыком. active в
  -- shared API — состояние навигации секции, а не тон курсора home,
  -- поэтому здесь он не передаётся: выделение курсора выражено явной
  -- заливкой selected и живёт ТОЛЬКО когда центральный буфер держит фокус
  -- (focusBuf==2). Отогнутый угол (focused) — тот же конъюнкт. При фокусе
  -- очереди (focusBuf==3) ни заливки, ни угла нет.
  -- Высота и арт — из GEO (cardH 180 = подвес 14 + слот угла 14 + тело
  -- 152; cardArt 96): числа считаются один раз в recomputeLayout и здесь
  -- только читаются, второй копии нет.
  if KU then
    return hintTarget(skin.kumihimoTag({
      key = "hf-card-" .. shelfIndex .. "-" .. cardIndex,
      width = HOME_CARD_WIDTH,
      height = GEO.cardH,
      gap = 3,
      padding = 7,
      -- Стабильный слот угла вырезается из height и существует всегда —
      -- фокус лишь проявляет клапан, геометрия не двигается. Курсор
      -- (заливка selected + угол) живёт ТОЛЬКО когда центральный буфер
      -- держит фокус (focusBuf==2); при фокусе очереди (3) ни того, ни
      -- другого нет.
      coverFold = true,
      fill = (active and focusBuf == 2) and COLORS.selected or KT.colors.surfaceVariant,
      focused = active and focusBuf == 2,
      onClick = function()
        homeShelf = shelfIndex
        homeItem = cardIndex
        focusBuf = 2
        draw()
      end
    }, {
      -- Явный квадратный слот: строка РОВНО 96 px, арт центрируется в ней
      -- и остаётся квадратом 96×96. Прямой ребёнок колонки растянулся бы
      -- по её ширине (Flex мерит поперечную ось точно) и превратился бы в
      -- широкий кроп обложки.
      ui.row({
        key = "hf-art-" .. shelfIndex .. "-" .. cardIndex,
        height = GEO.cardArt,
        align = "center",
        justify = "center",
      }, {
        artNode(c.art, GEO.cardArt),
        ui.row({ key = "hf-action-slot", width = 22 }, { badge }),
      }),
      -- Подписи живут в ФИКСИРОВАННЫХ flex-слотах 18/16 (нативный
      -- Label.height игнорируется), текст центрируется по вертикали,
      -- one-line без счётчика символов — границей служит maxWidth.
      ui.row({ key = "t-w", height = 18, align = "center" }, {
        searchLabel(c.title, {
          key = "t",
          size = 12,
          color = COLORS.text,
          maxLines = 1,
          maxWidth = HOME_CARD_WIDTH - 14,
        }, c.kind == "track" and c.title or "", identity .. ":title"),
      }),
      ui.row({ key = "s-w", height = 16, align = "center" }, {
        searchLabel(c.subtitle, {
          key = "s",
          size = 11,
          color = COLORS.muted,
          maxLines = 1,
          maxWidth = HOME_CARD_WIDTH - 14,
        }, c.artist, identity .. ":artist"),
      }),
    }), identity, badge)
  end
  return hintTarget(ui.column({
    key = "hf-card-" .. shelfIndex .. "-" .. cardIndex,
    width = HOME_CARD_WIDTH,
    height = 174,
    gap = 6,
    padding = 8,
    radius = 6,
    shape = SC and "chamfer" or nil,
    fill = active and COLORS.selected or COLORS.surface,
    onClick = function()
      homeShelf = shelfIndex
      homeItem = cardIndex
      draw()
    end,
  }, {
    ui.row({ key = "hf-art-slot", height = 110, align = "center", justify = "center" }, {
      artNode(c.art, 110),
      ui.row({ key = "hf-action-slot", width = 22 }, { badge }),
    }),
    searchLabel(shorten(c.title, 18), { key = "t", height = 16, color = COLORS.text },
      c.kind == "track" and c.title or "", identity .. ":title"),
    searchLabel(shorten(c.subtitle, 22), { key = "s", height = 16, color = COLORS.muted },
      c.artist, identity .. ":artist"),
  }), identity, badge)
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
    -- Один и тот же конец среза для подписи и цикла: pager не может
    -- показать диапазон шире реально отрисованных карточек (1–3/1 при
    -- одной карточке исключён).
    local cend = math.min(left + HOME_CARD_WINDOW - 1, #shelf.cards)
    local header = ui.row({ key = "hf-head-" .. i, height = 20, gap = 8, align = "center" }, compact(
      lbl(shorten(shelf.title, 24), {
        key = "hf-shelf-title-" .. i,
        size = 13,
        height = 20,
        color = COLORS.text,
        maxLines = KU and 1 or nil,
      }),
      shelf.subtitle ~= "" and lbl(shorten(shelf.subtitle, 28), {
        key = "hf-shelf-sub-" .. i,
        size = 11,
        height = 20,
        color = COLORS.muted,
        maxLines = KU and 1 or nil,
      }) or nil,
      lbl("", { key = "hf-spacer-" .. i, grow = 1 }),
      lbl("◄ " .. left .. "–" .. cend .. "/" .. shelf.count .. " ►", {
        key = "hf-range-" .. i,
        size = 10,
        color = COLORS.muted,
        mono = true,
        maxLines = KU and 1 or nil,
      })
    ))
    local cards = {}
    for j = left, cend do
      table.insert(cards, homeCardNode(i, j, shelf.cards[j]))
    end
    if KU then
      -- Полка: заголовок 20, зазор 6 до шнура, шнур 8, зазор 0 до ряда
      -- карточек. Неравные зазоры выражены обёрткой: внешняя колонка даёт
      -- 6 (gap=6), её внутренний напарник с gap=0 склеивает шнур с рядом.
      -- Итого 20+6+8+0+180 = 214.
      table.insert(shelfNodes, ui.column({ key = "hf-shelf-" .. i, height = HOME_SHELF_HEIGHT, gap = 6 }, {
        header,
        ui.column({ key = "hf-shelf-b-" .. i, gap = 0 }, {
          skin.kumihimoCord({ key = "hf-shelf-cord-" .. i }),
          ui.row({ key = "hf-cards-" .. i, gap = GAP }, cards),
        }),
      }))
    else
      table.insert(shelfNodes, ui.column({ key = "hf-shelf-" .. i, height = HOME_SHELF_HEIGHT, gap = 8 }, {
        header,
        ui.row({ key = "hf-cards-" .. i, gap = GAP }, cards),
      }))
    end
  end
  return ui.scroll({ key = "hf-scroll", flexGrow = 1 }, {
    ui.column({ key = "hf-body", gap = GAP }, shelfNodes),
  })
end

local function homeSectionNode()
  return ui.column({ key = "hf-section", gap = 8, flexGrow = 1 }, {
    ui.row({ key = "hf-controls", height = 28, gap = 8, align = "center" }, {
      lbl("Recommendations", { key = "hf-label", size = 12, grow = 1 }),
      lbl("Refresh", { key = "hf-refresh-label", size = 11, color = COLORS.muted }),
      ui.button({
        key = "hf-refresh",
        glyph = "refresh",
        controlSize = "sm",
        variant = "ghost",
        onClick = function() poke("home-refresh") end,
      }),
    }),
    homeFeedNode(),
  })
end

local function playlistCards(slice, first)
  local cards = {}
  for k, row in ipairs(slice) do
    local i = first + k - 1
    local title = lbl(shorten(row.title, 20), { key = "pl-title" .. i, height = 16, color = COLORS.text })
    table.insert(cards, ui.column({
      key = "pl" .. i,
      hintIdentity = "playlist:" .. i .. ":" .. tostring(row.plid),
      hintAnchor = title,
      width = 168,
      height = CARD_ROW_HEIGHT,
      gap = 8,
      padding = 12,
      radius = 6,
      shape = SC and "chamfer" or nil,
      fill = i == cursor and COLORS.selected or COLORS.surface,
      onClick = function()
        cursor = i
        openPlaylist(row)
        draw()
      end,
    }, {
      artNode(row.art, 120),
      title,
      lbl(shorten(row.subtitle, 24), { key = "pl-sub" .. i, height = 16, color = COLORS.muted }),
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
  if hints:isActive() then return end
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
    ui.row({ key = "q-head", gap = 8, align = "center", height = KU and 20 or nil }, {
      lbl("Queue", {
        key = "q-title",
        size = 12,
        color = focusBuf == 3 and COLORS.accent or COLORS.muted,
      }),
      lbl(queueCursor .. "/" .. #queue, {
        key = "q-count",
        size = 11,
        color = COLORS.muted,
        grow = 1,
        mono = true,
      }),
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
      local rowChildren = {
        ui.row({ key = "q-mark" .. i, width = 24, align = "center" }, {
          lbl(isPlaying and "▶" or tostring(i), {
            key = "q-idx" .. i,
            color = isPlaying and COLORS.accent or COLORS.muted,
            mono = true,
          }),
        }),
        -- Бейдж провайдера перед артом: из какого клиента трек.
        ui.row({ key = "q-badge" .. i, width = 22, align = "center" }, {
          lbl(providerGlyph(providerByTrack(t)), { key = "q-badge-t" .. i, color = COLORS.muted }),
        }),
        artNode(t.art_path, 28),
        ui.column({ key = "q-text" .. i, gap = 1, flexGrow = 1 }, {
          searchLabel(marker .. tostring(t.title or ""), {
            key = "q-title" .. i, size = 12,
            color = isPlaying and COLORS.accent or (isCursor and COLORS.text or COLORS.muted),
            maxLines = 1,
          }, t.title, "queue-title:" .. i .. ":" .. idOf(t)),
          searchLabel(type(t.artists) == "table" and table.concat(t.artists, ", ") or "", {
            key = "q-artist" .. i, size = 10, color = COLORS.muted, maxLines = 1,
          }, type(t.artists) == "table" and table.concat(t.artists, ", ") or "",
            "queue-artist:" .. i .. ":" .. idOf(t)),
        }),
        -- Длительность Kumihimo — ограниченный слот 44 (label.width хост
        -- игнорирует, поэтому ширину держит flex-обёртка).
        KU and ui.row({ key = "q-dur-w" .. i, width = 44, justify = "end", align = "center" }, {
          lbl(durationLabel(t.duration) or "", {
            key = "q-dur" .. i,
            size = 11,
            color = COLORS.muted,
            mono = true,
            maxLines = 1,
          }),
        }) or lbl(durationLabel(t.duration) or "", {
          key = "q-dur" .. i,
          width = 44,
          color = COLORS.muted,
          mono = true,
        }),
      }
      -- Клик мышью по строке очереди = намерение работать с ней:
      -- курсор и фокус буфера 3, иначе ввод уйдёт в центральный вид.
      local onClick = function()
        queueCursor = i
        followPlaying = false
        focusBuf = 3
        draw()
      end
      if KU then
        -- Подвесной ряд: подвес декоративен, тело ярлыка — цель клика.
        -- Играющий маркер (фонарь + ▶/акцент) независим от курсора.
        -- Курсор очереди — навигационный акцент active (тёплая рамка
        -- тела) плюс заливка selected, а НЕ отогнутый угол: угол-обложка
        -- ставится только центральному буферу (focusBuf==2), очередь —
        -- отдельная компактная колонка, а не шестой таб.
        table.insert(rowsOut, hintTarget(skin.kumihimoTag({
          key = "q" .. i,
          height = QUEUE_ROW_HEIGHT,
          gap = 0,
          padding = 7,
          playing = isPlaying,
          active = isCursor,
          fill = isCursor and COLORS.selected or nil,
          onClick = onClick,
        }, {
          ui.row({ key = "q-in" .. i, gap = 8, align = "center", flexGrow = 1 }, rowChildren),
        }), "queue:" .. i .. ":" .. idOf(t)))
      else
        table.insert(rowsOut, ui.row({
          key = "q" .. i,
          hintIdentity = "queue:" .. i .. ":" .. idOf(t),
          gap = 8,
          paddingH = 8,
          paddingV = 4,
          height = QUEUE_ROW_HEIGHT,
          radius = 6,
          shape = SC and "chamfer" or nil,
          align = "center",
          fill = isPlaying and COLORS.playing or (isCursor and COLORS.selected or COLORS.bg),
          border = (isCursor and isPlaying) and COLORS.border or nil,
          borderWidth = (isCursor and isPlaying) and 1 or nil,
          onClick = onClick,
        }, rowChildren))
      end
    end
    body = ui.column({ key = "q-rows", gap = QUEUE_ROW_GAP }, rowsOut)
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
      hintIdentity = "eq:" .. i .. ":" .. text,
      onClick = function()
        cursor = i
        focusBuf = 2
        if row.kind ~= "eq-band" then activate(row, i) end
        draw()
      end,
      gap = 8,
      paddingH = 10,
      paddingV = 4,
      radius = 6,
      shape = SC and "chamfer" or nil,
      fill = active and COLORS.selected or COLORS.surface,
    }, {
      lbl(text, {
        key = "eq-text-" .. i,
        grow = 1,
        color = active and COLORS.text or COLORS.muted,
        mono = true,
      }),
    }))
  end
  return ui.column({ key = "eq-lines", gap = 4 }, lines)
end

local function chipNode(kind, title)
  local active = searchKind == kind
  local props = KUF.warmButton({
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
  }, active)
  -- Kumihimo-фильтр: фиксированная высота 28, малый радиус 4 и рамка 1;
  -- активный — акцентный (hot), неактивный — тихий outline-материал.
  if KU then
    props.height = 28
    props.radius = 4
    props.borderWidth = 1
  end
  return ui.button(props)
end

-- Чип фильтра по клиенту: тот же вид, что у чипов вида, но переключение
-- чисто локальное — перевыдача не нужна, выдача уже в snapshot.
local function providerChipNode(pid, title)
  local active = searchProvider == pid
  local props = KUF.warmButton({
    key = "chip-prov-" .. pid,
    text = title,
    controlSize = "sm",
    variant = active and "primary" or "ghost",
    onClick = function()
      searchProvider = pid
      cursor = 1
      draw()
    end,
  }, active)
  if KU then
    props.height = 28
    props.radius = 4
    props.borderWidth = 1
  end
  return ui.button(props)
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

-- Пункты пикера: «+ New playlist…» (создаёт ЛОКАЛЬНЫЙ плейлист) +
-- доступные адресата. Локальные плейлисты — всегда; нативные — только
-- при совпадении провайдера плейлиста с провайдером трека и
-- caps.playlist_add (playlistAddable). Пересчитывается на каждый
-- вызов: snapshot мог обновиться, пока пикер открыт.
local function pickerItems(s)
  local trackProv = trackProvider(plPicker and plPicker.track or "")
  local out = { { title = "+ New playlist…" } }
  for _, p in ipairs(s.playlists or {}) do
    if playlistAddable(p, trackProv) then
      local ref = plRef(p)
      table.insert(out, { ref = ref, title = tostring(p.title or ""), liked = ref == "local:liked" })
    end
  end
  return out
end

-- Поповер «в плейлист»: отдельного оверлей-слоя у panel.render нет,
-- поэтому рисуем карточку выбора вместо правой context-колонки —
-- это самый дешёвый «поповер» без нового API. Состояние в plPicker
-- (track + cursor), пункты — pickerItems (локальные всегда, нативные
-- по capabilities).
local function playlistPickerNode(s)
  local items = pickerItems(s)
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
      hintIdentity = "picker:" .. tostring(plPicker.track) .. ":" .. tostring(item.ref or "new"),
      gap = 8,
      paddingH = 10,
      paddingV = 6,
      radius = 6,
      shape = SC and "chamfer" or nil,
      align = "center",
      fill = active and COLORS.selected or COLORS.surface,
      onClick = function()
        plPicker.cursor = i
        draw()
      end,
    }, {
      -- Общий Liked Music управляется оценками, а не pl add.
      lbl(item.liked and (shorten(item.title, 26) .. " · ♥ лайк") or shorten(item.title, 34), {
        key = "plpick-t" .. i,
        grow = 1,
        color = active and COLORS.accent or COLORS.text,
      }),
    }))
  end
  return ui.column({ key = "pl-picker", gap = 6, flexGrow = 3 }, out)
end

-- Строка карты подсказок: фиксированная высота ряда, клавиша акцентом
-- в чипе фикс. ширины, действие приглушённое и ОБРЕЗАНО до бюджета —
-- перенос внутри ряда невозможен (ширина текста гарантирована
-- shorten + фиксированной высотой ряда).
local HELP_KEY_WIDTH = 100
local HELP_ROW_HEIGHT = 22
local HELP_TEXT_BUDGET = 18
local function helpRow(id, keys, text)
  return ui.row({
    key = "help-" .. id,
    gap = 8,
    align = "center",
    height = HELP_ROW_HEIGHT,
  }, {
    lbl(keys, {
      key = "help-k-" .. id,
      width = HELP_KEY_WIDTH,
      size = 11,
      color = COLORS.accent,
      mono = true,
    }),
    lbl(shorten(text, HELP_TEXT_BUDGET), { key = "help-t-" .. id, grow = 1, size = 11, color = COLORS.muted }),
  })
end

-- Три компактные смысловые группы вместо двух плотных колонок:
-- Навигация, Поиск, Плеер и библиотека. Каждая — равная колонка;
-- максимум 9 рядов фиксированной высоты, описание короче бюджета,
-- поэтому группы помещаются в центральную панель без переноса.
local function helpGroup(key, title, rows)
  local children = { lbl(title, { key = "help-h-" .. key, size = 12, color = COLORS.text }) }
  for _, r in ipairs(rows) do
    table.insert(children, helpRow(key .. "-" .. r[1], r[2], r[3]))
  end
  return ui.column({ key = "help-col-" .. key, gap = 4, flexGrow = 1 }, children)
end

-- Модал подсказок: перекрывает центральные виды (паттерн plPicker),
-- rail/очередь/now-playing остаются на месте — спектр живёт и под
-- модалом. Статичен: одна страница, без скролла и курсора.
local function helpNode()
  return ui.column({ key = "help-modal", gap = GAP, flexGrow = 1 }, {
    lbl("Keymap", { key = "help-title", size = 18, color = COLORS.text }),
    ui.row({ key = "help-cols", gap = GAP, flexGrow = 1 }, {
      helpGroup("nav", "Навигация", {
        { "buf", "Tab · ctrl+h/l", "буферы 2↔3" },
        { "views", "ctrl+j/k · 1–5", "секции" },
        { "move", "j/k · стрелки", "список/сетка" },
        { "sides", "h/l · ←/→", "влево · вправо" },
        { "ends", "g", "начало · конец" },
        { "back", "BackSpace · Esc", "назад · закрыть" },
      }),
      helpGroup("search", "Поиск", {
        { "ins", "/ · i · .", "ввод (insert)" },
        { "cur", "←/→", "курсор" },
        { "edge", "Home · End", "начало/конец" },
        { "bs", "BackSpace", "стереть слева" },
        { "del", "Delete", "стереть справа" },
        { "kill", "ctrl+BackSpace", "стереть слово" },
        { "paste", "ctrl+v", "вставить из буфера" },
        { "enter", "Return", "искать" },
        { "esc", "Esc · jj · jk", "в normal" },
        { "prov", "c", "фильтр клиента" },
      }),
      helpGroup("player", "Плеер и библиотека", {
        { "pause", "Space", "пауза" },
        { "np", "n · p", "трек ±" },
        { "rs", "r · s", "repeat · shuffle" },
        { "hints", "F → метка", "действия / автор / название" },
        { "vol", "< · >", "громкость" },
        { "rate", "F → ♥ · d", "лайк · дизлайк" },
        { "pl", "a · D", "плейлист/удал." },
        { "flat", "0", "eq: Flat" },
        { "tui", "o", "терминал TUI" },
        { "hid", "ctrl+d", "дизлайки" },
      }),
    }),
    lbl("любая клавиша — закрыть · медиа-клавиши работают", {
      key = "help-close",
      size = 11,
      color = COLORS.muted,
    }),
  })
end

-- Витрина поисковой строки: before / каретка / after как отдельные
-- метки фиксированной высоты. Каретка — символ под курсором акцентом,
-- в конце строки — «▌»; окно queryLeft..+SEARCH_VIEW_CHARS сдвигается
-- так, чтобы каретка оставалась видимой. Побочного роста строки нет:
-- суммарная ширина сегментов ограничена окном в SEARCH_VIEW_CHARS+1
-- код-поинтов.
local function searchTextNode()
  local len = charCount(query)
  if queryLeft > queryCursor then
    queryLeft = queryCursor
  end
  if queryCursor >= queryLeft + SEARCH_VIEW_CHARS then
    queryLeft = queryCursor - SEARCH_VIEW_CHARS + 1
  end
  if queryLeft < 0 then
    queryLeft = 0
  end
  local before = queryLeft < queryCursor
    and string.sub(query, byteOffset(query, queryLeft), byteOffset(query, queryCursor) - 1)
    or ""
  local caretText, caretColor
  if queryCursor < len then
    caretText = string.sub(query, byteOffset(query, queryCursor), byteOffset(query, queryCursor + 1) - 1)
    caretColor = COLORS.accent
  else
    caretText = "▌"
    caretColor = typing and COLORS.accent or COLORS.muted
  end
  -- Хвост обрезается правой границей видимого окна: before+каретка+
  -- after не могут превысить SEARCH_VIEW_CHARS код-поинтов.
  local after = queryCursor < len
    and string.sub(query, byteOffset(query, queryCursor + 1),
      byteOffset(query, math.min(len, queryLeft + SEARCH_VIEW_CHARS)) - 1)
    or ""
  return ui.row({ key = "search-text", gap = 0, align = "center", flexGrow = 1 }, {
    lbl(before, { key = "search-before", color = COLORS.text }),
    lbl(caretText, { key = "search-caret", color = caretColor, mono = true }),
    lbl(after, { key = "search-after", grow = 1, color = COLORS.text }),
  })
end

function draw()
  -- Одно чтение рецепта на кадр. Семьи взаимоисключающи: Kumihimo
  -- проверяем первым, иначе stagecraft(). Токены — тоже на кадр, чтобы
  -- смена рецепта подхватывалась на ближайшем рендере. Бюджеты окон
  -- пересчитываются под активную семью (смена/очистка — тоже здесь).
  KU = skin.kumihimo()
  if KU then
    SC = false
    KT = skin.kumihimoTokens()
    KUF.kumihimoPalette(KT)
    -- view приходит параметром: функция объявлена до объявления `view`.
    KUF.recomputeLayout(true, KT.pad, view)
  else
    KT = nil
    SC = skin.stagecraft()
    if SC then
      CHAM = skin.tokens().chamfer
    end
    KUF.rolePalette()
    KUF.recomputeLayout(false, nil, view)
  end
  local s = snapshot
  local items = rows()
  -- Окно правой колонки-очереди клампится вместе с центральным
  -- списком, чтобы followPlaying/ручной курсор не вылезали за границы.
  clampQueue(s.queue or {})
  local slice, first
  if KU and view == "search" then
    -- Kumihimo Search: окно считает KUF.searchSlice по РЕАЛЬНЫМ узлам —
    -- результат 44, заголовок группы 18, зазор 6; сверху ввод 36, ряд
    -- фильтров 32 и (при наличии) ряд клиентов 32 со своими зазорами GAP.
    local budget = GEO.viewH - (36 + 32 + 2 * GAP)
    for _, p in ipairs(s.providers or {}) do
      if tostring(p.id or "") ~= "" then
        budget = budget - (32 + GAP)
        break
      end
    end
    first, slice = KUF.searchSlice(items, budget)
  else
    clampCursor(items)
    slice, first = visibleWindow(items)
  end

  local wsChildren = nil
  -- Единая правая колонка очереди для всех видов; отдельные context
  -- больше не дублируют её в каждой ветке.
  local context = queueContextNode(s)
  local workspaceGrow = 7
  local contextGrow = 3

  if view == "home" then
    -- Хиро исчезает без трека; npNode внизу остаётся всегда.
    wsChildren = {}
    -- Kumihimo: трек-хиро дублировал бы нижний транспорт на том же
    -- экране (один now-playing на вид) — под Kumihimo лента полок идёт
    -- сразу, без второго блока. Дефолт сохраняет хиро как раньше.
    if not KU then
      local hero = homeWorkspace(s)
      if hero then
        table.insert(wsChildren, hero)
      end
    end
    -- Лента рекомендаций: состояния/полки, прокручиваемый скролл.
    table.insert(wsChildren, homeSectionNode())
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
        -- Kumihimo: фиксированная строка 36 (6+24+6), chamfer 8, паддинг
        -- 10/6 и outline-рамка, которая теплеет до accent только когда
        -- ввод реально идёт (typing). Вне KU — прежние паддинги без рамки.
        paddingH = KU and 10 or 12,
        paddingV = KU and 6 or 8,
        height = KU and 36 or nil,
        radius = 8,
        shape = (KU or SC) and "chamfer" or nil,
        border = KU and (typing and COLORS.accent or KT.colors.outline) or nil,
        borderWidth = KU and 1 or nil,
        fill = COLORS.surface,
        onClick = function()
          focusBuf = 2
          typing = true
          insertPending = nil
          volumeKeys = false
          endHold()
          draw()
        end,
      }, {
        ui.glyph({
          key = "search-glyph",
          name = typing and "pencil" or "search",
          size = 14,
          color = typing and COLORS.accent or COLORS.muted,
        }),
        -- Витрина: текст с видимой кареткой (см. searchTextNode);
        -- пустой не-ввод — подсказка. Label-сегменты перерисовываются
        -- каждым draw() — текст живой по определению.
        (query ~= "" or typing)
          and searchTextNode()
          or lbl("i — ввод · j/k — навигация", {
            key = "search-placeholder",
            grow = 1,
            color = COLORS.muted,
          }),
        lbl(typing and "INSERT" or "NORMAL", {
          key = "search-mode",
          size = 10,
          color = typing and COLORS.accent or COLORS.muted,
        }),
      }),
      ui.row({ key = "chips", gap = 8, align = "center", height = KU and 32 or nil }, chips),
      ui.column({ key = "ws-body", gap = GAP, flexGrow = 1 }, body),
    }
    -- Строка чипов клиентов (§19) — между чипами видов и телом выдачи,
    -- только когда есть из чего выбирать (>1 чипа = All + клиенты).
    if #provChips > 1 then
      table.insert(wsChildren, 3, ui.row({ key = "chips-prov", gap = 8, align = "center", height = KU and 32 or nil }, provChips))
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
    local refreshError = tostring(s.tracksError or "")
    if refreshError ~= "" then
      table.insert(body, lbl(refreshError, {
        key = "playlist-refresh-error", size = 11, color = COLORS.error,
        maxLines = KU and 2 or nil,
      }))
    end
    if state then
      table.insert(body, state)
    else
      table.insert(body, ui.scroll({ key = "playlist-scroll-" .. first, flexGrow = 1 }, {
        ui.column({ key = "playlist-list", gap = 4 }, listChildren(slice, first)),
      }))
    end
    wsChildren = {
      ui.row({ key = "playlist-controls", height = 28, gap = 8, align = "center" }, compact(
        lbl(shorten(playlistTitle, 40), { key = "ws-title", size = 18, color = COLORS.text, grow = 1 }),
        (playlistId == "local:liked" or playlistId == "ytmusic:LM") and
          lbl(s.tracksLoading and "Refreshing…" or "Refresh", {
            key = "playlist-refresh-label", size = 11, color = COLORS.muted,
          }) or nil,
        (playlistId == "local:liked" or playlistId == "ytmusic:LM") and ui.button({
          key = "playlist-refresh",
          hintIdentity = playlistId .. ":refresh",
          enabled = not s.tracksLoading,
          glyph = s.tracksLoading and "loader" or "refresh",
          controlSize = "sm",
          variant = "ghost",
          onClick = function()
            if not snapshot.tracksLoading then
              poke("tracks-refresh", playlistId)
            end
          end,
        }) or nil
      )),
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
  -- У Kumihimo транспорт вынесен под main во всю ширину (см. корень),
  -- поэтому в workspace его не дублируем.
  if not KU then
    table.insert(wsChildren, npNode(s))
  end
  local workspace = ui.column({ key = "ws", gap = GAP, flexGrow = workspaceGrow }, wsChildren)

  -- Поповер выбора плейлиста перекрывает обычный context в любом виде.
  if plPicker then
    context = playlistPickerNode(s)
  end

  -- Материал рабочей области: активная Стейджикрафт-семья рисует
  -- контекстную колонку тем же шамфером и серебряным волоском.
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
      radius = SC and CHAM or 12,
      shape = SC and "chamfer" or nil,
      fill = COLORS.surface,
      border = (SC or KU) and COLORS.border or nil,
      borderWidth = (SC or KU) and 1 or nil,
    }, { context }),
  })

  -- Полная карта клавиш переехала в модал ? (приёмка 19.09: строка
  -- на весь этаж читалась плохо); футер оставляет только якоря и
  -- текущее состояние ввода.
  -- Бюджет футера (GEO.footer = 16) держится явной высотой строки и
  -- однострочными подписями — только у Kumihimo: у прочих семейств
  -- футер остаётся натуральной высоты.
  local footer = ui.row({
    key = "footer-row",
    gap = 8,
    align = "center",
    height = KU and GEO.footer or nil,
  }, {
    lbl(volumeKeys and "h/l · ←/→ — громкость · Esc — выход" or "F — действия · ? — карта · Tab — буферы", {
      key = "footer",
      size = 11,
      color = COLORS.muted,
      maxLines = KU and 1 or nil,
    }),
    lbl("", { key = "footer-spacer", grow = 1 }),
    lbl(volumeKeys and "VOLUME" or (typing and "INSERT" or "NORMAL"), {
      key = "footer-mode",
      size = 10,
      color = typing and COLORS.accent or COLORS.muted,
      mono = true,
      maxLines = KU and 1 or nil,
    }),
  })

  -- Корень. Kumihimo: горизонтальные вкладки вместо рейла, транспорт
  -- (шнур-акцент + real now-playing) во всю ширину под main. Остальные
  -- семьи: шапка → тело (сайдбар · workspace · очередь) → футер. Всё
  -- внутри рамки скина; бюджет рамки учтён в GEO.ringX/ringY.
  if KU then
    -- Явно ограниченный нижний транспорт: высота бюджета GEO.transport
    -- (fixed-height шнур-ряд + зазор 6 + np-карточка 110), поэтому он не
    -- растягивает корень и не выдавливает виды. Шнур в колонке НЕ растёт:
    -- flexGrow не задаём — ширину даёт stretch ряда, высоту — сам ряд.
    local transport = ui.column({ key = "np-full", gap = 6, height = GEO.transport }, {
      skin.kumihimoCord({ key = "np-cord" }),
      npNode(s),
    })
    renderPanel(skin.frame(ui.column({
      key = "root",
      gap = GAP,
      -- Корень Kumihimo паддится неодинаково: 16 по горизонтали, 8 по
      -- вертикали — ровно как вычитает GEO.bodyW/bodyH.
      paddingH = 16,
      paddingV = 8,
      fill = COLORS.bg,
      flexGrow = 1,
    }, {
      KUF.tabs(),
      main,
      transport,
      footer,
    }), "tmus-root"))
    return
  end
  renderPanel(skin.frame(ui.column({
    key = "root",
    gap = GAP,
    padding = PAD,
    fill = COLORS.bg,
    flexGrow = 1,
  }, {
    headerNode(s),
    ui.row({ key = "body", gap = GAP, flexGrow = 1 }, { railNode(), main }),
    footer,
  }), "tmus-root"))
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

function endHold()
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
    -- Удержание BackSpace в поле поиска: темп — как у стрелок,
    -- удаление слева от каретки по границе код-поинта.
    query, queryCursor = deleteBefore(query, queryCursor)
    return true
  elseif action == "delete-fwd" then
    -- Удержание Delete: удаление справа от каретки.
    query, queryCursor = deleteAt(query, queryCursor)
    return true
  elseif action == "cur-left" then
    queryCursor = math.max(0, queryCursor - 1)
    return true
  elseif action == "cur-right" then
    queryCursor = math.min(charCount(query), queryCursor + 1)
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
  local slow = held.action == "delete" or held.action == "delete-fwd"
    or held.action == "cur-left" or held.action == "cur-right"
  local interval = isMediaAction(held.action) and MEDIA_REPEAT_MS
    or (slow and DELETE_REPEAT_MS or NAV_REPEAT_MS)
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
  local pasteChord = string.lower(chord) == "ctrl+v" or string.lower(chord) == "ctrl+cyrillic_em"
  if not pressed then
    if chord == "Return" then searchEnterDown = false end
    if pasteChord then searchPasteDown = false end
    hints:release(chord)
    if held and held.chord == chord then
      endHold()
    end
    return
  end
  -- A submit changes insert to normal; its held press must not activate a row.
  if chord == "Return" and searchEnterDown then return end
  if pasteChord and searchPasteDown then return end
  if hints:isDown(chord) then return end


  if held and held.chord == chord then
    return
  end

  local key = LAYOUT[chord] or chord
  local lcHint = string.lower(tostring(chord))
  local through = lcHint == "tab" or lcHint == "ctrl+j" or lcHint == "ctrl+k"
    or lcHint == "ctrl+h" or lcHint == "ctrl+l"
    or lcHint == "ctrl+cyrillic_o" or lcHint == "ctrl+cyrillic_el"
    or lcHint == "ctrl+cyrillic_er" or lcHint == "ctrl+cyrillic_de"
    or chord == "XF86AudioNext" or chord == "XF86AudioPrev"
  if hints:isStale() then
    if through then
      hints:cancel()
    elseif lcHint == "escape" then
      hints:cancel()
      draw()
      return
    elseif Hint.letter(chord) ~= "f" then
      return
    end
  end
  if hints:isActive() then
    if through then
      hints:cancel()
    else
      hints:press(chord)
      draw()
      return
    end
  end
  if not plNaming and not (typing and focusBuf == 2) and Hint.letter(chord) == "f" then
    endHold()
    volumeKeys = false
    hints:start(chord)
    draw()
    return
  end
  if volumeKeys then
    if through then
      volumeKeys = false
    elseif lcHint == "escape" then
      volumeKeys = false
      draw()
      return
    elseif key == "h" or key == "l" or chord == "Left" or chord == "Right" then
      poke((key == "h" or chord == "Left") and "vol-down" or "vol-up")
      return
    else
      volumeKeys = false
    end
  end

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
      local ch = textChar(chord)
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
    local items = pickerItems(snapshot)
    local count = #items
    if chord == "Escape" then
      plPicker = nil
    elseif chord == "Return" then
      if plPicker.cursor == 1 then
        -- «+ New playlist…»: плейлист создаётся ЛОКАЛЬНЫМ (сервис
        -- шлёт `pl new <title>` без --provider), трек добавляется в
        -- цепочке там же — id знает только сервис.
        local track = plPicker.track
        plPicker = nil
        startPlNaming(track)
      else
        local item = items[plPicker.cursor]
        if item and item.ref then
          if item.ref == "local:liked" then
            -- Виртуальный плейлист меняется через оценку трека.
            poke("rate", "like:" .. plPicker.track)
          else
            poke("pl", "add:" .. item.ref .. ":" .. plPicker.track)
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
  -- запрос на позицию курсора, Esc или быстрые пары jj/jk (физические
  -- j/k; на ЙЦУКЕН это о/л) выходят в normal; «oo» эскейпом не
  -- является. typing=false — normal:
  -- j/k и стрелки листают выдачу, Enter активирует строку, i/слэш/
  -- точка возвращают insert. Медиа-клавиши работают в обеих.
  -- Поиск живёт только при фокусе буфера 2: в буфере 3 (очередь)
  -- буквы и хорды идут в его карту, а не в запрос.
  if inputMode and focusBuf == 2 then
    local isJ = chord == "J" or chord == "Cyrillic_o"
    local isK = chord == "K" or chord == "Cyrillic_el"
    if typing then
      -- Детект эскейп-пары jj/jk: первый j уже вставлен в запрос —
      -- при подтверждении пары он снимается. Окно 250 мс, как
      -- repeat_delay системной клавиатуры. «oo» эскейпом больше не
      -- является: обе «o» остаются текстом.
      if insertPending and (noctalia.nowMs() - insertPending.at) <= INSERT_ESCAPE_MS then
        if insertPending.key == "j" and (isJ or isK) then
          query, queryCursor = deleteBefore(query, queryCursor)
          typing = false
          queryCursor = charCount(query)
          insertPending = nil
          draw()
          return
        end
      end
      insertPending = nil
      if chord == "Return" then
        -- Insert Enter only submits. Playback requires a new press in normal.
        searchEnterDown = true
        if noctalia.string.trim(query) ~= "" then
          doSearch()
        end
        typing = false
        queryCursor = charCount(query)
        insertPending = nil
      elseif chord == "Escape" then
        typing = false
        queryCursor = charCount(query)
        insertPending = nil
      elseif pasteChord then
        -- One insertion per physical press, using the host's current text
        -- selection (nil for an empty/non-text clipboard). Never submit here.
        searchPasteDown = true
        local text = noctalia.clipboardText()
        local room = QUERY_MAX_CHARS - charCount(query)
        if type(text) == "string" and room > 0 then
          text = string.gsub(text, "%c+", " ")
          local count = utf8.len(text)
          if count and count >= 0 then
            if count > room then
              text = string.sub(text, 1, byteOffset(text, room) - 1)
            end
            query, queryCursor = insertAt(query, queryCursor, text)
          end
        end
      elseif chord == "Left" then
        -- Курсор ходит по код-поинтам; удержание — темп текста.
        queryCursor = math.max(0, queryCursor - 1)
        startHold(chord, "cur-left")
      elseif chord == "Right" then
        queryCursor = math.min(charCount(query), queryCursor + 1)
        startHold(chord, "cur-right")
      elseif chord == "Home" then
        queryCursor = 0
      elseif chord == "End" then
        queryCursor = charCount(query)
      elseif lc == "ctrl+backspace" then
        -- Удалить слово слева от курсора (см. killWordBefore). Без
        -- автоповтора сознательно: лаг одного кадра съел бы полстроки.
        query, queryCursor = killWordBefore(query, queryCursor)
      elseif chord == "BackSpace" or chord == "Backspace" then
        query, queryCursor = deleteBefore(query, queryCursor)
        startHold(chord, "delete")
      elseif chord == "Delete" then
        query, queryCursor = deleteAt(query, queryCursor)
        startHold(chord, "delete-fwd")
      elseif chord == "Up" then
        move(-1)
        startHold(chord, "up")
      elseif chord == "Down" then
        move(1)
        startHold(chord, "down")
      else
        -- Вставка В ПОЗИЦИЮ КУРСОРА (Space — тот же текстовый символ).
        -- Лимит буфера — по код-поинтам, не по байтам.
        local ch = chord == "Space" and " " or textChar(chord)
        if ch and charCount(query) < QUERY_MAX_CHARS then
          query, queryCursor = insertAt(query, queryCursor, ch)
          if isJ then
            insertPending = { key = "j", at = noctalia.nowMs() }
          end
        end
      end
    else
      if chord == "Return" then
        activate(rows()[cursor], cursor)
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
      activate(rows()[cursor], cursor)
    end
  elseif chord == "Space" then
    if view == "playlist" or view == "eq" then
      activate(rows()[cursor], cursor)
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
      -- Удаление из плейлиста. Локальный плейлист — по АБСОЛЮТНОЙ
      -- позиции (pl rm-at): дубликаты уходят строго по одному, идентичность
      -- не размывается. Нативный — только при caps.playlist_remove,
      -- иначе по id трека (pl rm).
      if row and row.kind == "track" and row.id and row.pos ~= nil
        and playlistId ~= "local:liked" then
        local prov = trackProvider(playlistId)
        if isLocalProvider(prov) then
          poke("pl", "rmat:" .. playlistId .. ":" .. tostring(row.pos))
        else
          local caps = capsOfProvider(prov)
          if type(caps) == "table" and caps.playlist_remove == true then
            local ref
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
        end
      end
    elseif view == "library" then
      local row = rows()[cursor]
      if row and row.kind == "playlist" and (row.plid or "") ~= ""
        and row.plid ~= "local:liked" then
        -- Обычные локальные плейлисты редактируемы; виртуальный — нет.
        local caps = capsOfProvider(row.provider)
        if isLocalProvider(row.provider)
          or (type(caps) == "table" and caps.playlist_delete == true) then
          poke("pl", "del:" .. row.plid)
        end
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
  hints:reset()
  panel.setCaptureCancel(false)
  volumeKeys = false
  plPicker = nil
  plNaming = false
  plName = ""
  plAddAfterCreate = nil
  helpShown = false
  insertPending = nil
  searchEnterDown = false
  searchPasteDown = false
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
  searching = true
  poke("search", (SEARCH_KIND_TOKENS[searchKind] or "all") .. "|" .. noctalia.string.urlEncode(query))
  draw()
end

function submitSearch(text)
  local textQuery = noctalia.string.trim(text or "")
  if textQuery == "" then return end
  endHold()
  setView("search")
  query = textQuery
  queryCursor = charCount(query)
  queryLeft = 0
  searchKind, searchProvider = "all", "all"
  focusBuf = 2
  typing = false
  insertPending = nil
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
    -- Лента догрузилась до конца окна при открытом home: если хвост
    -- уже на экране — запросить продолжение сразу, не дожидаясь хода.
    maybeHomeMore()
  end
  draw()
end)
