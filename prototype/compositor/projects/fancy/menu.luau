-- The launcher, as `fancy` shows it: an iron plate that comes up out of the
-- fire.
--
-- What it does (the applications, the matching, the order, starting one) is
-- the built-in `launcher` module's; this file is how it looks and moves.
--
--   Opening, the plate springs up from a little below and a little
--   smaller, red hot, and cools to a warm glow while it is open. Every key
--   typed flares it again and stirs the embers behind everything.
--   Closing, it cools to grey iron and sinks away.
--   The row that is chosen lies on a hot bar, which travels on a spring
--   from row to row. Each application has a mark with its first letter in
--   place of an icon.
--
-- Keys while it is open: typing narrows the list, BackSpace takes a
-- character away (Ctrl: a word; Ctrl+U: everything), Up / Down (Ctrl+P /
-- Ctrl+N, Tab) choose, Return starts the one chosen, Escape or logo+D
-- closes. Every key goes to the launcher, none to the windows. A click on a
-- row starts it; a click beside the plate closes it.

local affine = hz.include("affine")
local spring = hz.include("spring")
local launcher = hz.include("launcher")
local paint = hz.include("paint")

local M = {}

M.settings = {
    rows = 8,
    -- Logical pixels.
    width = 640,
    row_height = 46,
    header = 66,
    footer = 30,
    padding = 12,
    radius = 18,
    -- How far down the screen the plate sits (share of the height).
    top = 0.16,
    -- Springs: { omega, zeta }.
    rise = { 15, 0.62 },
    sink = { 9, 1 },
    heat = { 10, 1 },
    cool = { 13, 1 },
    bar = { 28, 0.78 },
    grow = { 20, 0.8 },
    -- The list is never shorter than this many rows.
    least_rows = 3,
    -- sRGB colors.
    colors = {
        plate = { 0.075, 0.072, 0.080 },
        field = { 0.030, 0.030, 0.036 },
        text = { 0.93, 0.90, 0.86 },
        dim = { 0.55, 0.55, 0.60 },
        hot = { 1.0, 0.56, 0.20 },
        hotter = { 1.0, 0.86, 0.55 },
        ember = { 0.92, 0.25, 0.07 },
        cold = { 0.42, 0.45, 0.52 },
        -- The marks of the applications: iron, bronze, ember and steel.
        marks = {
            { 0.62, 0.24, 0.12 },
            { 0.66, 0.46, 0.24 },
            { 0.32, 0.36, 0.44 },
            { 0.48, 0.30, 0.22 },
            { 0.26, 0.40, 0.42 },
            { 0.56, 0.52, 0.46 },
        },
    },
}

local menu = launcher.new({ rows = M.settings.rows })
local colors = {}
local font = nil
-- Springs: `show` (0 closed .. 1 open; swings past on the way up), `heat`
-- (1 red hot .. 0 grey), `bar` (the row of the hot bar, from 1 at the top).
local show, heat, bar
-- How many rows tall the list is: it grows and shrinks with the matches.
local length
local flare_at = -math.huge
local clock = -math.huge
-- Where the last frame drew the plate and its rows (global logical), for
-- clicks.
local drawn = nil

local function now()
    local t = hz.time()
    if t > clock then
        clock = t
    end
    return clock
end

local function new_spring(value, kind)
    local s = M.settings[kind]
    return spring.new(value, s[1], s[2], 0.002)
end

-- Aims a spring, moving the way `kind` says from here on.
local function aim(s, target, kind, t)
    spring.aim(s, target, t)
    local k = M.settings[kind]
    s.omega, s.zeta = k[1], math.min(k[2], 1)
end

function M.init(saved)
    for name, c in M.settings.colors do
        if name ~= "marks" then
            colors[name] = paint.color(c[1], c[2], c[3])
        end
    end
    colors.marks = {}
    for i, c in M.settings.colors.marks do
        colors.marks[i] = paint.color(c[1], c[2], c[3])
    end
    font = hz.load_font and hz.load_font() or nil
    -- The applications are read once now (some tens of milliseconds for a
    -- few hundred files), so that the first logo+D does not wait for it;
    -- opening reads them again only when their directories changed.
    pcall(hz.desktop_entries)
    menu = launcher.new({ rows = M.settings.rows, memory = saved and saved.memory })
    show = new_spring(0, "rise")
    heat = new_spring(0, "heat")
    bar = new_spring(1, "bar")
    length = new_spring(M.settings.rows, "grow")
    drawn = nil
end

function M.save()
    return { memory = menu.memory }
end

function M.is_open()
    return menu.open
end

local function flare()
    flare_at = now()
    hz.request_frame()
end

function M.open()
    if menu.open then
        return
    end
    launcher.open(menu)
    local t = now()
    -- Up from below, red hot; the bar is at its row already.
    spring.snap(show, 0)
    aim(show, 1, "rise", t)
    spring.snap(heat, 1.25)
    aim(heat, 1, "heat", t)
    spring.snap(bar, 1)
    spring.snap(length, math.clamp(#menu.results, M.settings.least_rows, M.settings.rows))
    flare()
end

function M.close()
    if not menu.open then
        return
    end
    launcher.close(menu)
    local t = now()
    aim(show, 0, "sink", t)
    aim(heat, 0, "cool", t)
    hz.request_frame()
end

function M.toggle()
    if menu.open then
        M.close()
    else
        M.open()
    end
end

-- The row of the bar follows the chosen row.
local function follow(t)
    aim(bar, menu.selected - menu.top + 1, "bar", t)
end

-- A key while the launcher is open. Returns what happened (see the
-- `launcher` module), and stirs the fire for it.
function M.key(keycode, pressed, mods, k)
    local what = launcher.key(menu, keycode, pressed, mods, k)
    local t = now()
    if what == "typed" then
        spring.snap(bar, 1)
        flare()
    elseif what == "moved" then
        follow(t)
        hz.request_frame()
    elseif what == "started" or what == "closed" then
        aim(show, 0, "sink", t)
        aim(heat, 0, "cool", t)
        if what == "started" then
            -- Gone into the fire: a last flare.
            flare_at = t
        end
        hz.request_frame()
    end
    return what
end

-- A click at (x, y) (global logical) while the launcher is open: on a row,
-- it starts it; beside the plate, it closes the launcher.
function M.click(x, y)
    if not drawn then
        M.close()
        return
    end
    for _, row in drawn.rows do
        if x >= row.x and x < row.x + row.w and y >= row.y and y < row.y + row.h then
            launcher.start_at(menu, row.index)
            flare_at = now()
            aim(show, 0, "sink", now())
            aim(heat, 0, "cool", now())
            hz.request_frame()
            return
        end
    end
    local p = drawn.plate
    if not (x >= p.x and x < p.x + p.w and y >= p.y and y < p.y + p.h) then
        M.close()
    end
end

-- Whether (x, y) is on the plate (input there goes to no window).
function M.covers(x, y)
    local p = drawn and drawn.plate
    return p ~= nil and menu.open and x >= p.x and x < p.x + p.w and y >= p.y and y < p.y + p.h
end

local function mix(a, b, k)
    return { a[1] + (b[1] - a[1]) * k, a[2] + (b[2] - a[2]) * k, a[3] + (b[3] - a[3]) * k }
end

local function scaled(c, k)
    return { c[1] * k, c[2] * k, c[3] * k }
end

-- A stable color for an application's mark.
local function mark_color(id)
    local h = 0
    for i = 1, #id do
        h = (h * 31 + string.byte(id, i)) % 65521
    end
    return colors.marks[h % #colors.marks + 1]
end

-- Text that fits `width` logical pixels at `height`, cut short with an
-- ellipsis when it does not.
local ELLIPSIS = "…"
local function fit(text, height, size, width)
    local w = paint.text_width(text, height, size)
    if w <= width then
        return text, w
    end
    local ellipsis = ELLIPSIS
    if font and hz.missing_glyphs and hz.missing_glyphs(font, ELLIPSIS) > 0 then
        ellipsis = "..."
    end
    local count = launcher.length(text)
    local keep = math.max(1, math.floor(count * width / w) - 2)
    local cut = launcher.cut(text, keep) .. ellipsis
    return cut, paint.text_width(cut, height, size)
end

-- What the logo key is called, for the hints.
local function logo_name()
    local key = hz.logo_key and hz.logo_key() or "super"
    if key == "alt" then
        return "Alt"
    elseif key == "ctrl" then
        return "Ctrl"
    end
    return "Super"
end

-- Adds the launcher to a scene of `stage` (`scene.overlay`), as it is at
-- time t. Asks for another frame while anything moves.
function M.frame(t, output, scene)
    if t > clock then
        clock = t
    end
    local s = M.settings
    -- A key held down repeats.
    local repeated = launcher.repeat_keys(menu, t)
    if repeated == "typed" then
        spring.snap(bar, 1)
        flare_at = t
    elseif repeated == "moved" then
        aim(bar, menu.selected - menu.top + 1, "bar", t)
    end
    local k, _, moving_show = spring.step(show, t)
    local h, _, moving_heat = spring.step(heat, t)
    local row_at, _, moving_bar = spring.step(bar, t)
    aim(length, math.clamp(#menu.results, s.least_rows, s.rows), "grow", t)
    local rows_tall, _, moving_length = spring.step(length, t)
    local opacity = math.clamp(k, 0, 1)
    if opacity <= 0.002 and not menu.open then
        drawn = nil
        return
    end
    local fresh = math.exp(-math.max(t - flare_at, 0) / 0.22)
    local moving = moving_show or moving_heat or moving_bar or moving_length or fresh > 0.01 or menu.held ~= nil
    h = math.clamp(h, 0, 1.4)

    local sx, sy, sw, sh = output.x, output.y, output.logical_width, output.logical_height
    local scale = output.scale
    local width = math.min(s.width, sw - 48)
    local rows = launcher.visible(menu)
    local list_height = rows_tall * s.row_height
    local height = s.padding + s.header + s.padding + list_height + s.footer + s.padding
    local x = sx + (sw - width) / 2
    local y = sy + math.max(24, sh * s.top)
    -- Rising: from 26 px below and 0.94 of the size, past the place and
    -- back; sinking: down and smaller again.
    local lift = 1 - k
    local plate_scale = 0.94 + 0.06 * k
    local transform = affine.chain(
        affine.translate(x, y + 26 * lift),
        affine.about(width / 2, height / 2, affine.scale(plate_scale))
    )
    local phase = (scene.backdrop and scene.backdrop.phase or 0) + t * 0.6 * math.min(h, 1) * (if moving then 1 else 0)
    local warm = math.min(h, 1)
    local rim = mix(colors.cold, colors.hot, warm)
    local overlay = { frames = {}, labels = {}, frost = scene.frost }
    scene.overlay = overlay

    local function frame(fx, fy, fw, fh, look)
        table.insert(overlay.frames, {
            transform = affine.multiply(transform, affine.translate(fx, fy)),
            width = fw,
            height = fh,
            look = look,
        })
    end
    local function label(text, lx, ly, lh, color, alpha, align)
        local px, py = affine.apply(transform, lx, ly)
        table.insert(overlay.labels, {
            text = text,
            x = px,
            y = py,
            height = lh * plate_scale,
            size = math.max(8, math.floor(lh * scale + 0.5)),
            color = color,
            alpha = alpha * opacity,
            align = align or "left",
        })
    end

    -- The plate.
    frame(0, 0, width, height, {
        radius = s.radius,
        margin = 90,
        fill = colors.plate,
        fill_alpha = 0.82,
        frost = if scene.frost then 1 else 0,
        frost_level = 3.2,
        rim = rim,
        line = 0.35 + 0.65 * warm,
        line_width = 1.5 + 0.8 * warm,
        glow_color = colors.hotter,
        glow = warm * (0.32 + 0.9 * fresh) + 0.35 * math.max(h - 1, 0),
        glow_width = 7 + 10 * fresh + 8 * math.max(h - 1, 0),
        flow = warm,
        phase = phase,
        shadow = 0.62,
        softness = 34,
        drop = 18,
        opacity = opacity,
    })

    -- The field for what is typed.
    local fx, fy, fw, fh = s.padding, s.padding, width - 2 * s.padding, s.header
    frame(fx, fy, fw, fh, {
        radius = 12,
        margin = 30,
        fill = colors.field,
        fill_alpha = 0.78,
        rim = mix(colors.cold, colors.hot, warm * (0.35 + 0.65 * fresh)),
        line = 0.25 + 0.5 * fresh,
        line_width = 1.2,
        glow_color = colors.hot,
        glow = 0.5 * fresh * warm,
        glow_width = 6,
        opacity = opacity,
    })
    local text_height = 30
    local tx, ty = fx + 20, fy + fh / 2
    if menu.query == "" then
        label("Type to search", tx + 8, ty, text_height, colors.dim, 0.75)
    else
        local shown, shown_width = fit(menu.query, text_height, math.floor(text_height * scale + 0.5), fw - 60)
        label(shown, tx, ty, text_height, scaled(colors.text, 1 + 0.6 * fresh), 1)
        tx += shown_width + 3
    end
    -- The caret: a short bar of hot iron where the next letter goes.
    frame(tx, ty - 13, 2.5, 26, {
        radius = 1.2,
        margin = 10,
        fill = colors.hotter,
        fill_alpha = 0.6 + 0.4 * warm,
        glow_color = colors.hot,
        glow = 0.6 * warm,
        glow_width = 4,
        opacity = opacity,
    })
    -- How many match, at the right of the field.
    label(string.format("%d / %d", #menu.results, #menu.apps), fx + fw - 18, ty, 18, colors.dim, 0.8, "right")

    -- The rows.
    local lx, ly = s.padding, s.padding + s.header + s.padding
    local rows_drawn = {}
    if #rows > 0 then
        -- The hot bar under the chosen row, on its spring.
        local bar_y = ly + (row_at - 1) * s.row_height
        bar_y = math.clamp(bar_y, ly - s.row_height * 0.3, ly + (s.rows - 0.7) * s.row_height)
        frame(lx, bar_y + 3, width - 2 * s.padding, s.row_height - 6, {
            radius = 10,
            margin = 40,
            fill = mix(colors.cold, colors.ember, warm),
            fill_alpha = 0.09 + 0.07 * warm,
            rim = rim,
            line = 0.55 + 0.4 * warm,
            line_width = 1.4,
            glow_color = colors.hotter,
            glow = 0.38 * warm + 0.2 * fresh,
            glow_width = 8,
            flow = warm,
            phase = phase,
            opacity = opacity,
        })
    end
    for place, row in rows do
        local ry = ly + (place - 1) * s.row_height
        local app = row.app
        local mid = ry + s.row_height / 2
        -- The mark: the first letter, on a plate of the application's color.
        local mark = 30
        local mx = lx + 12
        frame(mx, mid - mark / 2, mark, mark, {
            radius = 8,
            margin = 12,
            fill = mark_color(app.id),
            fill_alpha = if row.chosen then 1 else 0.82,
            rim = if row.chosen then colors.hotter else colors.cold,
            line = if row.chosen then 0.7 else 0.25,
            line_width = 1,
            shadow = 0.4,
            softness = 4,
            drop = 2,
            opacity = opacity,
        })
        label(launcher.initial(app, font), mx + mark / 2, mid, 22, colors.text, 0.95, "center")
        local name = launcher.label(app, font)
        local name_x = mx + mark + 16
        local room = width - 2 * s.padding - (name_x - lx) - 16
        local name_height = 24
        local shown, shown_width = fit(name, name_height, math.floor(name_height * scale + 0.5), room * 0.62)
        local bright = if row.chosen then 1.15 else 0.92
        label(shown, name_x, mid, name_height, scaled(colors.text, bright), 1)
        -- What it is, dimmer, after the name.
        local what = app.generic_name or app.comment
        if what and font and hz.missing_glyphs and hz.missing_glyphs(font, what) > 0 then
            what = nil
        end
        if what then
            local rest = room - shown_width - 18
            if rest > 60 then
                local small = 17
                local detail = fit(what, small, math.floor(small * scale + 0.5), rest)
                label(detail, name_x + shown_width + 18, mid + 1, small, colors.dim, if row.chosen then 0.95 else 0.7)
            end
        end
        local gx, gy = affine.apply(transform, lx, ry)
        local gx2, gy2 = affine.apply(transform, width - s.padding, ry + s.row_height)
        table.insert(rows_drawn, { index = row.index, x = gx, y = gy, w = gx2 - gx, h = gy2 - gy })
    end
    if #rows == 0 then
        label(if #menu.apps == 0 then "No applications found" else "Nothing matches", width / 2, ly + list_height / 2, 22, colors.dim, 0.8, "center")
    end

    -- The keys, along the bottom.
    local hint = "Return  start      Esc  close      Up / Down  choose      " .. logo_name() .. "+D  close"
    label(hint, width / 2, height - s.padding - s.footer / 2 + 2, 15, colors.dim, 0.7, "center")

    local px, py = affine.apply(transform, 0, 0)
    local px2, py2 = affine.apply(transform, width, height)
    drawn = { plate = { x = px, y = py, w = px2 - px, h = py2 - py }, rows = rows_drawn }
    if moving then
        hz.request_frame()
    end
end

-- For tools and tests: what the launcher shows.
function M.describe()
    local list = {}
    for _, row in launcher.visible(menu) do
        table.insert(list, { id = row.app.id, name = row.app.name, chosen = row.chosen, index = row.index })
    end
    return {
        open = menu.open,
        query = menu.query,
        selected = menu.selected,
        count = #menu.results,
        total = #menu.apps,
        rows = list,
        drawn = drawn,
        show = show and (spring.at(show, clock)) or 0,
    }
end

return M
