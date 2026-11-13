-- The launcher: a menu of the applications of the machine, on the screen of
-- the focused monitor.
--
-- Logo+D opens it on the focused monitor, and again (or Escape) closes it.
-- While it is open it has the keyboard: what is typed searches, Backspace
-- takes back a letter (with Ctrl, a word; Ctrl+U all), Up / Down (also
-- Ctrl+P / Ctrl+N, Shift+Tab / Tab, Page Up / Page Down, Home / End)
-- choose, Enter starts the chosen application and closes the menu. Keys
-- held down repeat. A click on a line of the menu starts that application;
-- a click anywhere else closes the menu.
--
-- What a launcher does (the list, the search, the order, how an
-- application is started, the keys inside the menu) is the built-in module
-- `launcher`, which other projects use too. This module is how the room
-- shows it: drawn into the monitor's own image, over what the monitor
-- shows (darkened). As the monitor lights the room, so does the menu, and
-- the line that is chosen colors the desk and the walls with the color of
-- its application. The camera moves a little closer to that monitor.

local launcher = hz.include("launcher")
local dots = hz.include("dots")

local M = {}

M.settings = {
    -- How far the camera moves towards the monitor of the menu: 0 not at
    -- all, 1 until the monitor fills the view.
    closer = 0.45,
    -- Seconds the menu takes to come up, and to go away.
    open_seconds = 0.16,
    close_seconds = 0.24,
    -- Lines of applications at most.
    rows = 8,
}

local KEY_D = 32

-- Called after anything changed that the camera or the picture depends on,
-- with the time it happened.
M.changed = function(_t) end
-- The time of an event (set by the window manager's clock).
M.now = function()
    return hz.time()
end
-- The monitor that has the focus (set by the window manager).
M.focused = function()
    return 1
end
-- The monitors of the scene (set by the window manager).
M.monitors = {}

-- The launcher's state (the built-in module's), and how the room shows it:
-- `monitor` is the index of the monitor it is on (also while it goes away).
local menu = launcher.new({ rows = M.settings.rows })
local state = {
    monitor = nil,
    opened_at = -math.huge,
    closed_at = -math.huge,
    launched = false,
}

local function ease(p)
    p = math.clamp(p, 0, 1)
    return p * p * (3 - 2 * p)
end

-- How far the menu is up (0..1) at time t, how bright the flash of a
-- start is (0..1), and whether that still changes.
function M.phase(t)
    if menu.open then
        local p = (t - state.opened_at) / M.settings.open_seconds
        return ease(p), 0, p < 1
    elseif state.monitor then
        local p = (t - state.closed_at) / M.settings.close_seconds
        return 1 - ease(p), if state.launched then 1 - math.clamp(p, 0, 1) else 0, p < 1
    end
    return 0, 0, false
end

-- The monitor the menu is drawn on at time t, or nil. Lets go of a menu
-- that has gone away.
function M.monitor_at(t)
    if not menu.open and state.monitor then
        local _, _, moving = M.phase(t)
        if not moving then
            state.monitor = nil
        end
    end
    return state.monitor
end

function M.is_open()
    return menu.open
end

-- The lines of the menu on a monitor, in its pixels.
function M.layout(monitor)
    local px, py = monitor.pixels_x, monitor.pixels_y
    local u = math.min(px, py) / 840
    local pad = 28 * u
    local header = 128 * u
    local footer = 60 * u
    local row_h = 76 * u
    local width = math.min(px - 120 * u, 1180 * u)
    local rows = math.clamp(math.floor((py * 0.86 - header - footer - pad) / row_h), 1, M.settings.rows)
    local height = header + rows * row_h + pad * 0.5 + footer
    local x0, y0 = math.round((px - width) / 2), math.round((py - height) / 2)
    return {
        u = u,
        panel = { x0, y0, math.round(width), math.round(height) },
        field = { x0 + math.round(pad), y0 + math.round(pad), math.round(width - 2 * pad), math.round(72 * u) },
        row_x = x0 + math.round(pad),
        row_w = math.round(width - 2 * pad),
        row_top = y0 + math.round(header),
        row_h = row_h,
        rows = rows,
        footer = { x0 + math.round(pad), math.round(y0 + height - footer), math.round(width - 2 * pad), math.round(footer) },
    }
end

function M.open()
    local monitor = M.monitors[M.focused()]
    state.monitor = M.focused()
    state.launched = false
    state.opened_at = M.now()
    -- As many lines as fit the monitor.
    menu.rows = if monitor then M.layout(monitor).rows else M.settings.rows
    launcher.open(menu)
    M.changed(state.opened_at)
end

-- After the menu closed (`launched`: it started an application).
local function closed(launched)
    state.launched = launched == true
    state.closed_at = M.now()
    M.changed(state.closed_at)
end

function M.close()
    if menu.open then
        launcher.close(menu)
        closed(false)
    end
end

-- Keys. Returns true for those the menu takes: logo+D always, every key
-- while it is open.
function M.key(keycode, pressed, mods, k)
    if mods.logo and keycode == KEY_D then
        if pressed then
            if menu.open then
                M.close()
            else
                M.open()
            end
        end
        return true
    end
    if not menu.open then
        return false
    end
    local what = launcher.key(menu, keycode, pressed, mods, k)
    if what == "started" or what == "closed" then
        closed(what == "started")
    elseif what then
        M.changed(M.now())
    end
    return true
end

-- Keys held down repeat: called from `frame`. Returns whether frames are
-- wanted for that.
function M.tick(t)
    if launcher.repeat_keys(menu, t) then
        M.changed(t)
    end
    return menu.open and menu.held ~= nil
end

-- The line of the menu under a point of its monitor (in its pixels): the
-- index into the results, or 0 inside the menu but on no line, or nil
-- outside it.
function M.line_at(monitor, x, y)
    local l = M.layout(monitor)
    local p = l.panel
    if x < p[1] or y < p[2] or x >= p[1] + p[3] or y >= p[2] + p[4] then
        return nil
    end
    if x < l.row_x or x >= l.row_x + l.row_w or y < l.row_top then
        return 0
    end
    local line = math.floor((y - l.row_top) / l.row_h) + 1
    if line > l.rows then
        return 0
    end
    local index = menu.top + line - 1
    return if menu.results[index] then index else 0
end

-- A click while the menu is open: on a line it starts that application;
-- off the menu, it closes. `monitor` and the point (in its pixels) are
-- what is under the pointer, or nil. Returns true when the click is the
-- menu's.
function M.click(monitor, x, y)
    if not menu.open then
        return false
    end
    local line = monitor and monitor.index == state.monitor and M.line_at(monitor, x, y)
    if line == nil or line == false then
        M.close()
    elseif line > 0 then
        menu.selected = line
        launcher.start_at(menu, line)
        closed(true)
    end
    return true
end

-- What was started, to keep over a reload of the script.
function M.save()
    return menu.memory
end

function M.restore(saved)
    menu = launcher.new({ rows = M.settings.rows, memory = saved })
    state.monitor = nil
end

-- For tests and tools: the middle of line `line` of the menu in view, in
-- the pixels of its monitor: { monitor index, x, y }, or nil.
function M.line_point(line)
    local monitor = state.monitor and M.monitors[state.monitor]
    if not monitor then
        return nil
    end
    local l = M.layout(monitor)
    return { monitor.index, l.row_x + l.row_w / 2, l.row_top + (line - 0.5) * l.row_h }
end

function M.describe()
    local names, ids = {}, {}
    for _, result in menu.results do
        table.insert(names, result.app.name)
        table.insert(ids, result.app.id)
    end
    return {
        open = menu.open,
        monitor = state.monitor or false,
        query = menu.query,
        selected = if #menu.results > 0 then menu.selected else false,
        results = ids,
        names = names,
        count = #menu.apps,
    }
end

-- ----- drawing ----------------------------------------------------------

local pipeline, sampler, blank_slot, blank_image, blank_written
local push = buffer.create(96)
local font = nil
-- What has been made into textures: key -> { texture, image, width,
-- height, used }.
local textures = {}
local generation = 0
local linear_srgb

-- `to_working(c)`: a color written as linear sRGB, in the working space.
function M.init(format, to_working)
    linear_srgb = to_working
    pipeline = gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", shader = "shaders/menu.slang", entry = "vs_main" }),
        fragment = gfx.create_shader({ stage = "fragment", shader = "shaders/menu.slang", entry = "fs_main" }),
        targets = { { format = format, blend = "premultiplied" } },
        topology = "triangle-strip",
    })
    sampler = gfx.create_sampler({ mag = "linear", min = "linear" })
    blank_image = gfx.create_image({ format = "r8unorm", width = 1, height = 1, usage = { "sampled", "copy_dst" } })
    blank_slot = gfx.create_texture_slot(blank_image)
    blank_written = false
    font = hz.load_font and hz.load_font() or nil
    textures = {}
    state.monitor = nil
end

local function forget(key)
    local made = textures[key]
    if made then
        if made.texture then
            gfx.destroy(made.texture)
            gfx.destroy(made.image)
        end
        textures[key] = nil
    end
end

-- A line of text at a size (pixels) as a texture, or nil (no font, or
-- none of its letters in the font).
local function text_of(text, size)
    if not font or text == "" then
        return nil
    end
    local key = "t" .. size .. ":" .. text
    local made = textures[key]
    if not made then
        local ok, rendered = pcall(hz.render_text, font, text, { size = size })
        made = if ok and rendered then rendered else { width = 0, height = 0 }
        textures[key] = made
    end
    made.used = generation
    return if made.texture then made else nil
end

-- Letters of dots as a texture: { texture, width } (width in dots).
local function dots_of(text)
    text = dots.normalize(text)
    local key = "d:" .. text
    local made = textures[key]
    if not made then
        local width, data = dots.pixels(text)
        local image = gfx.create_image({ format = "r8unorm", width = width, height = dots.HEIGHT, usage = { "sampled", "copy_dst" } })
        gfx.upload_image(image, data)
        made = { image = image, texture = gfx.create_texture_slot(image), width = width, height = dots.HEIGHT }
        textures[key] = made
    end
    made.used = generation
    return made
end

-- A color of an application, from its id: a hue, as linear sRGB.
local function hue_of(id)
    local h = 0
    for i = 1, #id do
        h = (h * 31 + string.byte(id, i)) % 1000003
    end
    local hue = (h % 360) / 360
    local k = { (hue * 6) % 6, (hue * 6 + 4) % 6, (hue * 6 + 2) % 6 }
    local rgb = {}
    for i = 1, 3 do
        -- A saturated hue, not quite pure: 0.12 .. 1.
        local c = math.clamp(math.abs(k[i] - 3) - 1, 0, 1)
        rgb[i] = 0.12 + 0.88 * c
    end
    return rgb
end

local function scaled(c, s)
    return { c[1] * s, c[2] * s, c[3] * s }
end

-- The first letter of an application for its mark (a letter the dots
-- have).
local function initial_of(app)
    for _, name in { launcher.label(app, font), app.default_name, string.match(app.exec[1] or "", "[^/]*$") } do
        local letter = string.match(name or "", "%w")
        if letter then
            return string.upper(letter)
        end
    end
    return "?"
end

local function logo_name()
    local key = hz.logo_key and hz.logo_key() or "super"
    return string.upper(string.sub(key, 1, 1)) .. string.sub(key, 2)
end

-- The colors the menu is drawn in, as linear sRGB.
local PALETTE = {
    accent = { 0.32, 0.72, 1.0 },
    panel = { 0.004, 0.005, 0.008 },
    field = { 0.012, 0.014, 0.02 },
    text = { 0.86, 0.86, 0.86 },
    faint = { 0.09, 0.09, 0.1 },
    dark = { 0.003, 0.003, 0.004 },
}

-- What the menu draws on `monitor` at time t: { ops, key, animated }, or
-- nil when the menu is not on that monitor. `key` says everything the
-- picture depends on (nil while it changes by itself). Made before the
-- monitor's pass begins: the textures it needs are made here.
function M.plan(monitor, t)
    if M.monitor_at(t) ~= monitor.index then
        return nil
    end
    generation += 1
    if not blank_written then
        gfx.upload_image(blank_image, buffer.create(1))
        blank_written = true
    end
    local alpha, flash, animated = M.phase(t)
    local l = M.layout(monitor)
    local u = l.u
    local ops = {}
    local function color(c, a)
        local r, g, b = linear_srgb(c)
        local s = (a or 1) * alpha
        return { r * s, g * s, b * s, s }
    end
    local function light(c, s)
        local r, g, b = linear_srgb(c)
        return { r * s * alpha, g * s * alpha, b * s * alpha, 0 }
    end
    local function box(rect, fill, options)
        options = options or {}
        table.insert(ops, {
            kind = 0,
            rect = rect,
            fill = fill,
            rim = options.rim or fill,
            glow = options.glow or { 0, 0, 0, 0 },
            reach = options.reach or 0,
            radius = options.radius or 0,
            rim_width = options.rim_width or 0,
        })
    end
    -- A line of text, its left end at x, its middle at y. Returns its
    -- width (0 when nothing was drawn).
    local function text(s, x, y, size, c, dot_size)
        if s == "" then
            return 0
        end
        if font then
            local made = text_of(s, math.round(size))
            if not made then
                return 0
            end
            table.insert(ops, { kind = 1, rect = { math.round(x), math.round(y - made.height / 2), made.width, made.height }, fill = c, texture = made.texture })
            return made.width
        end
        -- Without a font: dots, at about the same height.
        local d = dots_of(s)
        local dot = math.max(math.floor((dot_size or size / 9) + 0.5), 1)
        table.insert(ops, { kind = 2, rect = { math.round(x), math.round(y - dots.HEIGHT * dot / 2), d.width * dot, dots.HEIGHT * dot }, fill = c, texture = d.texture })
        return d.width * dot
    end
    local function dot_text(s, x, y, dot, c, align)
        local d = dots_of(s)
        local w = d.width * dot
        if align == "right" then
            x -= w
        elseif align == "center" then
            x -= w / 2
        end
        table.insert(ops, { kind = 2, rect = { math.round(x), math.round(y - dots.HEIGHT * dot / 2), w, dots.HEIGHT * dot }, fill = c, texture = d.texture })
    end

    local rise = math.round((1 - alpha) * 18 * u)
    local function lift(rect)
        return { rect[1], rect[2] + rise, rect[3], rect[4] }
    end

    -- What the monitor shows, darkened.
    box({ 0, 0, monitor.pixels_x, monitor.pixels_y }, { 0, 0, 0, 0.88 * alpha })
    -- The panel.
    box(lift(l.panel), color(PALETTE.panel, 0.94), {
        rim = color(scaled(PALETTE.accent, 0.14)),
        rim_width = math.max(2 * u, 1),
        radius = 20 * u,
        glow = light(PALETTE.accent, 0.05),
        reach = 40 * u,
    })
    -- The search line.
    local field = lift(l.field)
    box(field, color(PALETTE.field), {
        rim = color(scaled(PALETTE.accent, 0.4)),
        rim_width = math.max(2 * u, 1),
        radius = 12 * u,
    })
    local middle = field[2] + field[4] / 2
    local dot = math.max(math.round(5 * u), 1)
    dot_text(">", field[1] + 24 * u, middle, dot, color(PALETTE.accent))
    local query_x = field[1] + 70 * u
    local written = text(menu.query, query_x, middle, 36 * u, color(PALETTE.text), 5 * u)
    if menu.query == "" then
        text("Search", query_x + 14 * u, middle, 36 * u, color(PALETTE.faint), 5 * u)
    end
    -- The caret, after what is written.
    local caret_x = query_x + written + (if written > 0 then 6 * u else 0)
    box({ math.round(caret_x), math.round(middle - 22 * u), math.max(math.round(3 * u), 1), math.round(44 * u) }, color(scaled(PALETTE.accent, 1.2)), {
        glow = light(PALETTE.accent, 0.25),
        reach = 10 * u,
    })
    -- How many match, of how many.
    local small = math.max(math.round(3 * u), 1)
    dot_text(string.format("%d/%d", #menu.results, #menu.apps), field[1] + field[3] - 24 * u, middle, small, color(scaled(PALETTE.text, 0.25)), "right")

    -- The lines.
    local keys = {}
    for line, visible in launcher.visible(menu) do
        local app = visible.app
        table.insert(keys, app.id)
        local y = l.row_top + (line - 1) * l.row_h + rise
        local row = { l.row_x, math.round(y), l.row_w, math.round(l.row_h - 8 * u) }
        local mid = row[2] + row[4] / 2
        local hue = hue_of(app.id)
        local chosen = visible.chosen
        if chosen then
            local bright = 0.26 * (1 + 5 * flash)
            box(row, color(scaled(hue, bright)), {
                rim = color(scaled(hue, 1.1 * (1 + flash))),
                rim_width = math.max(2 * u, 1),
                radius = 12 * u,
                glow = light(hue, 0.22 * (1 + 2 * flash)),
                reach = 26 * u,
            })
        end
        -- The mark: the first letter on a square of the application's color.
        local mark = math.round(54 * u)
        local mark_rect = { row[1] + math.round(12 * u), math.round(mid - mark / 2), mark, mark }
        box(mark_rect, color(if chosen then PALETTE.dark else scaled(hue, 0.14)), {
            rim = color(scaled(hue, if chosen then 0.2 else 0.35)),
            rim_width = math.max(2 * u, 1),
            radius = 10 * u,
        })
        dot_text(initial_of(app), mark_rect[1] + mark / 2, mid, dot, color(scaled(hue, if chosen then 1.3 else 1.0)), "center")
        -- The name, and what it is.
        local name_x = mark_rect[1] + mark + math.round(22 * u)
        local ink = if chosen then scaled(PALETTE.text, 1.1) else PALETTE.text
        local about = app.generic_name or app.comment
        local name_y = if about then mid - 13 * u else mid
        -- (The name without a locale, when the font has not the letters
        -- of the translated one; always, in dots.)
        local label = if font then launcher.label(app, font) else app.default_name or app.name
        text(label, name_x, name_y, 31 * u, color(ink), 4 * u)
        if about then
            text(about, name_x, mid + 19 * u, 21 * u, color(if chosen then scaled(PALETTE.text, 0.6) else PALETTE.faint), 3 * u)
        end
        if app.terminal then
            dot_text("TERM", row[1] + row[3] - 20 * u, mid, small, color(scaled(PALETTE.text, if chosen then 0.6 else 0.3)), "right")
        end
    end
    if #menu.results == 0 then
        local message = if #menu.apps == 0 then "No applications" else "No match"
        text(message, l.row_x + 24 * u, l.row_top + l.row_h / 2 + rise, 28 * u, color(PALETTE.faint), 4 * u)
    end
    -- What the keys do.
    local footer = lift(l.footer)
    text(
        logo_name() .. "+D  close      Up / Down  choose      Enter  start      Esc  close",
        footer[1] + 8 * u,
        footer[2] + footer[4] / 2,
        20 * u,
        color(scaled(PALETTE.text, 0.2)),
        2 * u
    )

    -- Let go of what this menu no longer shows.
    for key, made in textures do
        if made.used ~= generation then
            forget(key)
        end
    end
    local key = nil
    if not animated then
        key = string.format(
            "menu:%s:%d:%d:%s:%s:%g",
            menu.query,
            menu.selected,
            menu.top,
            table.concat(keys, ","),
            logo_name(),
            alpha
        )
    end
    return { ops = ops, key = key, animated = animated }
end

-- Draws a plan, inside the pass of the monitor's image.
function M.draw(plan, width, height)
    gfx.set_pipeline(pipeline)
    gfx.bind(1, sampler)
    buffer.writef32(push, 80, width)
    buffer.writef32(push, 84, height)
    for _, op in plan.ops do
        local r = op.rect
        buffer.writef32(push, 0, r[1])
        buffer.writef32(push, 4, r[2])
        buffer.writef32(push, 8, r[3])
        buffer.writef32(push, 12, r[4])
        for i = 1, 4 do
            buffer.writef32(push, 16 + (i - 1) * 4, op.fill[i])
        end
        local rim = op.rim or op.fill
        local glow = op.glow or { 0, 0, 0, 0 }
        for i = 1, 4 do
            buffer.writef32(push, 32 + (i - 1) * 4, rim[i])
        end
        for i = 1, 3 do
            buffer.writef32(push, 48 + (i - 1) * 4, glow[i])
        end
        buffer.writef32(push, 60, op.reach or 0)
        buffer.writef32(push, 64, op.radius or 0)
        buffer.writef32(push, 68, op.rim_width or 0)
        buffer.writef32(push, 72, op.kind)
        buffer.writef32(push, 76, 0)
        gfx.push(0, push)
        gfx.bind(0, op.texture or blank_slot)
        gfx.draw(4)
    end
end

return M
