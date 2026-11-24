-- The stage: where everything is drawn at a given time.
--
-- `space` decides where windows belong. This module moves what is drawn
-- towards that, and describes the result for `paint`.
--
-- One strip. Every workspace lies on one long strip, side by side. A
-- window's place is a place on the strip: its workspace's stretch of it,
-- plus where its tile is. Sending a window to another workspace only gives
-- it a new place further along; it travels there like any other move.
--
-- One camera. The screen shows the strip through a camera: a position along
-- the strip and a zoom. Switching workspace moves the camera. While it
-- travels it pulls back a little, so that the workspaces pass by as plates
-- with the wall between them; standing still, it is exactly 1:1. For the
-- overview the camera pulls far back and the strip folds into rows, like a
-- line of text that wraps, until every workspace is in sight.
--
-- Springs. The camera and every window follow their targets on springs
-- (`spring`), which keep their speed when the target changes.
--
-- Heat. The window with the focus is hot: its rim glows, and the glow
-- flares when the focus arrives. A window that loses the focus cools down.
-- The embers of the backdrop and the flow of the rims are stirred by what
-- happens (a switch, a move, an opening window) and come to rest again.
-- When nothing moves, nothing is drawn.
--
-- Time is the time a frame is expected on screen (`frame`'s `time`); events
-- use `hz.time()`, never earlier than the last frame.

local affine = hz.include("affine")
local spring = hz.include("spring")
local space = hz.include("space")
local paint = hz.include("paint")

local G = {}

G.settings = {
    -- Whether the core is told which windows are out of view
    -- (`hz.set_visible`), so that their clients stop drawing. false keeps
    -- every window drawing, for comparison.
    hide_unseen = true,
    -- Springs: { omega (stiffness), zeta (damping) }. See `spring.luau`.
    camera = { 13, 1 },
    zoom = { 12, 1 },
    move = { 17, 0.86 },
    resize = { 17, 1 },
    open = { 15, 0.72 },
    fade = { 40, 1 },
    tilt = { 14, 0.6 },
    heat = { 9, 1 },
    -- Logical pixels of wall between two workspaces on the strip.
    gutter = 96,
    -- How far the camera pulls back while it travels (share of the size).
    pull = 0.13,
    -- While the camera travels, windows trail behind their workspace: the
    -- first by `trail` seconds of the camera's travel, each further one in
    -- the workspace's order by `trail_step` more.
    trail = 0.003,
    trail_step = 0.004,
    -- The camera speed (workspaces per second) the trails stop growing at.
    trail_limit = 3,
    -- Whether a window's contents are stretched to its rectangle while
    -- that changes size. Without it they stay as they are, from the top
    -- left corner, and the rectangle opens or closes over them (what a
    -- terminal wants: its text does not move).
    stretch = false,
    -- The overview: the share of the screen's width the workspaces take
    -- together, and the gap between two of them (share of the width).
    overview_width = 0.94,
    overview_gap = 0.022,
    -- A window opens from, and closes to, this scale.
    open_scale = 0.9,
    close_scale = 0.94,
    -- A window held by the pointer is drawn at this scale and opacity, and
    -- leans into its motion: by `held_tilt` radians at `held_speed` logical
    -- pixels per second and beyond.
    held_scale = 0.88,
    held_opacity = 1,
    held_tilt = 0.07,
    held_speed = 2600,
    -- Corner radius of a window, logical pixels.
    radius = 11,
    -- How bright a window without the focus is drawn (1: as it is). It
    -- cools down to this, and warms up again when the focus comes back.
    cold_brightness = 0.72,
    -- Whether windows, plates and the tray of the marks are frosted glass:
    -- what is behind them shows through blurred (a terminal with a clear
    -- background, a window that does not fill its tile).
    frost = true,
    -- How long the embers and rims keep moving after something happened
    -- (seconds to fall to about a third), and how strongly they are stirred.
    stir_time = 1.1,
    stir = 1,
    -- sRGB colors.
    colors = {
        wall_top = { 0.050, 0.056, 0.078 },
        wall_bottom = { 0.105, 0.088, 0.092 },
        ember = { 0.92, 0.25, 0.07 },
        spark = { 1.0, 0.70, 0.32 },
        hot = { 1.0, 0.56, 0.20 },
        hotter = { 1.0, 0.86, 0.55 },
        cold = { 0.42, 0.45, 0.52 },
        backing = { 0.055, 0.058, 0.070 },
        plate = { 0.10, 0.105, 0.13 },
        mark = { 0.50, 0.52, 0.58 },
    },
}

local camera, zoom -- springs: position on the strip in workspaces (1 = the first), overview 0..1
local entries = {} -- window id -> entry (see `entry_for`)
local clock = -math.huge
local present_at_start = {}
local colors = {} -- name -> linear working color
-- Stirring: `energy` at time `since`, falling off; `phase` is its integral.
local stir = { phase = 0, energy = 0, since = 0 }
-- What the last frame showed.
local shown = {
    view = affine.identity(),
    views = {},
    zoom = 1,
    camera = 1,
    pull = 0,
    overview = 0,
    moving = false,
    order = {},
}
-- What the core was last told about each window: whether it is seen.
local told = {}
-- The window the pointer holds: { id, dx, dy } (grab point in the window), or nil.
local held = nil
-- A camera position given by hand (a swipe in progress), or nil.
local handheld = nil

local function tick(t)
    if t > clock then
        clock = t
    end
    return clock
end

local function now()
    return tick(hz.time())
end

local function output_rect()
    local output = hz.outputs()[1]
    if not output then
        return 0, 0, 1, 1
    end
    return output.x, output.y, output.logical_width, output.logical_height
end

local function pitch()
    local _, _, width = output_rect()
    return width + G.settings.gutter
end

local function new_spring(value, kind, epsilon)
    local s = G.settings[kind]
    return spring.new(value, s[1], s[2], epsilon)
end

-- Stirring ---------------------------------------------------------------

local function stir_at(t)
    local tau = G.settings.stir_time
    local fall = math.exp(-math.max(t - stir.since, 0) / tau)
    return stir.phase + stir.energy * tau * (1 - fall), stir.energy * fall
end

-- Something happened: the embers and the rims move for a while.
function G.stir(amount)
    local t = now()
    local phase, energy = stir_at(t)
    stir.phase = phase
    stir.energy = math.min(energy + amount * G.settings.stir, 1.5)
    stir.since = t
    hz.request_frame()
end

-- Windows ----------------------------------------------------------------

-- Where a window belongs on the strip: x, y, w, h.
local function target_of(id)
    local rect = space.rect_of(id)
    local index = space.workspace_of(id)
    if not rect or not index or rect.w <= 0 or rect.h <= 0 then
        return nil
    end
    return rect.x + (index - 1) * pitch(), rect.y, rect.w, rect.h, rect.layer
end

local function sync(t)
    local s = G.settings
    local live = {}
    local changed = false
    for _, window in hz.windows() do
        local id = window.id
        local x, y, w, h, layer = target_of(id)
        if window.mapped and x then
            live[id] = true
            local entry = entries[id]
            if not entry then
                entry = {
                    x = new_spring(x, "move", 0.3),
                    y = new_spring(y, "move", 0.3),
                    w = new_spring(w, "resize", 0.3),
                    h = new_spring(h, "resize", 0.3),
                    scale = new_spring(1, "open", 0.0008),
                    opacity = new_spring(1, "fade", 0.004),
                    heat = new_spring(0, "heat", 0.004),
                    lift = new_spring(0, "open", 0.004),
                    fit = new_spring(0, "resize", 0.004),
                    tilt = new_spring(0, "tilt", 0.0005),
                    flare = -math.huge,
                    opened = -math.huge,
                }
                entries[id] = entry
                if not present_at_start[id] then
                    spring.place(entry.scale, s.open_scale, t)
                    spring.place(entry.opacity, 0, t)
                    entry.flare = t
                    entry.opened = t
                    G.stir(0.5)
                end
                changed = true
            end
            entry.layer = layer
            entry.focused = window.focused
            entry.workspace = space.workspace_of(id)
            -- The size of what the client drew.
            entry.content_w, entry.content_h = window.width or w, window.height or h
            if held and held.id == id then
                -- The pointer has it: `G.hold_to` says where it is.
            elseif entry.x.target ~= x or entry.y.target ~= y then
                spring.aim(entry.x, x, t)
                spring.aim(entry.y, y, t)
                changed = true
            end
            if entry.w.target ~= w or entry.h.target ~= h then
                spring.aim(entry.w, w, t)
                spring.aim(entry.h, h, t)
                changed = true
            end
            local hot = if window.focused then 1 else 0
            if entry.heat.target ~= hot then
                spring.aim(entry.heat, hot, t)
                if hot == 1 then
                    entry.flare = t
                end
                changed = true
            end
        end
    end
    for _, closed in hz.closed_windows() do
        live[closed.id] = true
        local entry = entries[closed.id]
        if not entry or not entry.drawn then
            -- Never drawn: nothing to fade out.
            hz.forget_closed(closed.id)
            entries[closed.id] = nil
        elseif not entry.closing then
            entry.closing = true
            spring.aim(entry.opacity, 0, t)
            spring.aim(entry.scale, s.close_scale, t)
            spring.aim(entry.heat, 0, t)
            changed = true
        end
    end
    for id in entries do
        if not live[id] then
            entries[id] = nil
        end
    end
    local view = handheld or space.active()
    if camera.target ~= view and not handheld then
        spring.aim(camera, view, t)
        G.stir(0.8)
        changed = true
    end
    return changed
end

-- The camera -------------------------------------------------------------

-- The transform from the strip to the screen for a camera position
-- (in workspaces), a zoom, and the screen (x, y, w, h).
local function view_of(position, scale, sx, sy, sw, sh)
    local cx, cy = sx + sw / 2, sy + sh / 2
    local fx = (position - 1) * pitch() + cx
    return { scale, 0, 0, scale, cx - scale * fx, cy - scale * cy }
end

-- The overview: where each workspace lies when all of them are in sight.
-- The strip folds into rows (two, from four workspaces on), each centered.
-- Returns for every workspace the transform from the strip to the screen.
local function overview_views(count, sx, sy, sw, sh)
    local s = G.settings
    local rows = if count > 3 then 2 else 1
    local columns = math.ceil(count / rows)
    local gap = sw * s.overview_gap
    -- Room at the bottom for the workspace marks.
    local room_h = sh - 44
    local scale = math.min(
        (sw * s.overview_width - gap * (columns - 1)) / (columns * sw),
        (room_h * 0.9 - gap * (rows - 1)) / (rows * sh)
    )
    local top = sy + (room_h - (rows * scale * sh + (rows - 1) * gap)) / 2
    local views = {}
    for index = 1, count do
        local row = (index - 1) // columns
        local column = (index - 1) % columns
        local in_row = math.min(columns, count - row * columns)
        local left = sx + (sw - (in_row * scale * sw + (in_row - 1) * gap)) / 2
        local x = left + column * (scale * sw + gap)
        local y = top + row * (scale * sh + gap)
        -- The workspace's corner on the strip goes to (x, y).
        views[index] = { scale, 0, 0, scale, x - scale * ((index - 1) * pitch() + sx), y - scale * sy }
    end
    return views
end

local function blend(a, b, k)
    if k <= 0 then
        return a
    elseif k >= 1 then
        return b
    end
    return {
        a[1] + (b[1] - a[1]) * k,
        0,
        0,
        a[4] + (b[4] - a[4]) * k,
        a[5] + (b[5] - a[5]) * k,
        a[6] + (b[6] - a[6]) * k,
    }
end

local function window_look(entry, state, t, phase)
    local s = G.settings
    if entry.layer == "full" then
        return nil
    end
    local heat = state.heat
    local flare = math.exp(-math.max(t - entry.flare, 0) / 0.28)
    local lift = state.lift
    local floating = entry.layer == "float"
    local softness = (if floating then 26 else 16) + 22 * lift + 6 * heat
    local drop = (if floating then 12 else 6) + 14 * lift + 2 * heat
    local rim = {
        colors.cold[1] + (colors.hot[1] - colors.cold[1]) * heat,
        colors.cold[2] + (colors.hot[2] - colors.cold[2]) * heat,
        colors.cold[3] + (colors.hot[3] - colors.cold[3]) * heat,
    }
    local glow_width = 5 + 9 * flare + 5 * lift
    return {
        radius = s.radius,
        margin = math.max(softness * 2 + drop, glow_width * 6) + 4,
        fill = colors.backing,
        -- Glass, so that a window with a clear background shows the wall;
        -- solid while the window opens, over whatever is still beneath it.
        fill_alpha = 0.55 + 0.45 * math.exp(-math.max(t - entry.opened, 0) / 0.2),
        frost = if s.frost then 1 else 0,
        frost_level = 2.5,
        rim = rim,
        line = 0.32 + 0.68 * heat,
        line_width = 1.5 + 0.5 * heat,
        glow_color = colors.hotter,
        glow = heat * (0.42 + 0.9 * flare) + flare * 0.35,
        glow_width = glow_width,
        flow = heat,
        phase = phase,
        shadow = (if floating then 0.55 else 0.42) + 0.2 * lift,
        softness = softness,
        drop = drop,
        opacity = state.opacity,
    }
end

-- The work of a frame at time t for the output: what to draw.
function G.frame(t, output)
    t = tick(t)
    sync(t)
    local s = G.settings
    local sx, sy, sw, sh = output.x, output.y, output.logical_width, output.logical_height
    local moving = false

    local position, speed, travelling = spring.step(camera, t)
    if handheld then
        position, speed, travelling = handheld, 0, false
    end
    local overview, _, zooming = spring.step(zoom, t)
    moving = travelling or zooming
    -- Pulled back while between two workspaces, or still fast.
    local between = math.sin(math.pi * (position - math.floor(position)))
    local pull = math.max(between * between, math.tanh(math.abs(speed) / 2.5))
    local travel_scale = 1 - s.pull * pull
    -- The camera's view, and with the overview each workspace's own: on
    -- its way to its place among the others.
    local view = view_of(position, travel_scale, sx, sy, sw, sh)
    local count = space.count()
    local views = {}
    if overview > 0 then
        for index, folded in overview_views(count, sx, sy, sw, sh) do
            views[index] = blend(view, folded, overview)
        end
    else
        for index = 1, count do
            views[index] = view
        end
    end
    local scale = views[math.clamp(math.floor(position + 0.5), 1, count)][1]
    -- The view for something at x on the strip: its workspace's, and in
    -- the wall between two workspaces, on the way from one to the other.
    local function view_at(x)
        if overview <= 0 then
            return view
        end
        local along = x - sx
        local index = math.clamp(along // pitch() + 1, 1, count)
        local past = along - (index - 1) * pitch() - sw
        if past <= 0 or index >= count then
            return views[index]
        end
        local k = math.clamp(past / s.gutter, 0, 1)
        return blend(views[index], views[index + 1], k * k * (3 - 2 * k))
    end
    -- How much each workspace is the one chosen: 1 where the camera is.
    local function chosen_of(index)
        return math.max(0, 1 - math.abs(position - index))
    end
    -- How far a window with a trail of one second is behind its workspace.
    local behind = s.trail_limit * math.tanh(speed / s.trail_limit) * pitch() * (1 - overview)
    local apart = math.max(pull, overview)

    local phase, energy = stir_at(t)
    if energy > 0.01 then
        moving = true
    end

    local scene = { frames = {}, windows = {}, marks = {}, labels = {}, frost = s.frost }
    scene.backdrop = {
        top = colors.wall_top,
        bottom = colors.wall_bottom,
        ember = colors.ember,
        ember_strength = 0.11 + 0.14 * apart,
        spark = colors.spark,
        spark_strength = 0.13 + 0.12 * apart,
        shift = (position - 1) * pitch() * 0.16,
        phase = phase,
        zoom = scale,
    }

    -- The plates of the workspaces that are in sight, once the camera has
    -- pulled back.
    local selected = space.active()
    local visible = {}
    for index = 1, count do
        local own = views[index]
        local left = own[1] * ((index - 1) * pitch() + sx) + own[5]
        if left < sx + sw and left + own[1] * sw > sx then
            visible[index] = true
            if apart > 0.002 then
                local chosen = chosen_of(index) * apart
                table.insert(scene.frames, {
                    transform = affine.multiply(own, affine.translate((index - 1) * pitch() + sx, sy)),
                    width = sw,
                    height = sh,
                    look = {
                        radius = 22,
                        margin = 110,
                        fill = colors.plate,
                        fill_alpha = 0.5 * apart,
                        frost = if s.frost then math.min(1, apart * 3) else 0,
                        frost_level = 3.5,
                        rim = {
                            colors.cold[1] + (colors.hot[1] - colors.cold[1]) * chosen,
                            colors.cold[2] + (colors.hot[2] - colors.cold[2]) * chosen,
                            colors.cold[3] + (colors.hot[3] - colors.cold[3]) * chosen,
                        },
                        line = (0.4 + 0.6 * chosen) * apart,
                        line_width = (1.5 + chosen) / own[1],
                        glow_color = colors.hot,
                        glow = 0.6 * chosen,
                        glow_width = 8 / own[1],
                        flow = chosen,
                        phase = phase,
                        shadow = 0.5 * apart,
                        softness = 30,
                        drop = 14,
                        opacity = 1,
                    },
                })
            end
        end
    end

    -- In the overview every workspace carries its number (the key that
    -- goes there), large and faint over what is on it.
    if overview > 0.02 then
        for index = 1, count do
            local own = views[index]
            local chosen = chosen_of(index)
            local cx, cy = affine.apply(own, (index - 1) * pitch() + sx + sw / 2, sy + sh / 2)
            table.insert(scene.labels, {
                text = tostring(index),
                x = cx,
                y = cy,
                height = own[1] * sh * 0.5,
                color = {
                    colors.mark[1] + (colors.hotter[1] - colors.mark[1]) * chosen,
                    colors.mark[2] + (colors.hotter[2] - colors.mark[2]) * chosen,
                    colors.mark[3] + (colors.hotter[3] - colors.mark[3]) * chosen,
                },
                alpha = overview * (0.22 + 0.33 * chosen),
            })
        end
    end

    -- Where the window in hand goes when it is let go: its tile, red hot.
    if held and entries[held.id] then
        local x, y, w, h = target_of(held.id)
        local lift = spring.at(entries[held.id].lift, t)
        if x and lift > 0.01 then
            table.insert(scene.frames, {
                transform = affine.multiply(view, affine.translate(x, y)),
                width = w,
                height = h,
                look = {
                    radius = s.radius,
                    margin = 60,
                    fill = colors.ember,
                    fill_alpha = 0.05,
                    rim = colors.hot,
                    line = 0.95,
                    line_width = 2,
                    glow_color = colors.hotter,
                    glow = 0.55,
                    glow_width = 9,
                    flow = 1,
                    phase = phase,
                    opacity = math.clamp(lift, 0, 1),
                },
            })
        end
    end

    -- Windows, workspace by workspace; the one in view last (on top), and
    -- within it the stacking order `space` gave.
    local order = {}
    local function add(id, entry, trail)
        local x, _, mx = spring.step(entry.x, t)
        local y, _, my = spring.step(entry.y, t)
        local w, _, mw = spring.step(entry.w, t)
        local h, _, mh = spring.step(entry.h, t)
        local k, _, mk = spring.step(entry.scale, t)
        local opacity, _, mo = spring.step(entry.opacity, t)
        local heat, _, mheat = spring.step(entry.heat, t)
        local lift, _, ml = spring.step(entry.lift, t)
        local tilt, _, mt = spring.step(entry.tilt, t)
        -- With `stretch`: what the client drew is stretched to the
        -- rectangle while that changes size (the client is asked for the new
        -- size at once, and answers within a frame or two). A client that
        -- settles for another size than its tile is shown as it is, from
        -- the top left corner.
        local cw, ch = math.max(entry.content_w or w, 1), math.max(entry.content_h or h, 1)
        local obeys = cw == entry.w.target and ch == entry.h.target
        spring.aim(entry.fit, if s.stretch and (mw or mh or obeys) then 1 else 0, t)
        local fit, _, mf = spring.step(entry.fit, t)
        local fit_x, fit_y = 1 + (w / cw - 1) * fit, 1 + (h / ch - 1) * fit
        local flaring = t - entry.flare < 1.2
        if mx or my or mw or mh or mk or mo or mheat or ml or mf or mt or flaring then
            moving = true
        end
        if entry.layer ~= "full" then
            x += behind * trail
        end
        opacity = math.clamp(opacity, 0, 1) * (1 - (1 - s.held_opacity) * math.clamp(lift, 0, 1))
        heat = math.clamp(heat, 0, 1)
        local state = { x = x, y = y, w = w, h = h, scale = k, opacity = opacity, heat = heat, lift = lift }
        entry.drawn = state
        -- Scaled about its middle; a window that was picked up, about the
        -- point it is held at, until it is back at its size.
        local pivot_x, pivot_y = w / 2, h / 2
        if entry.pivot then
            if not mk and k == 1 then
                entry.pivot = nil
            else
                pivot_x, pivot_y = entry.pivot[1], entry.pivot[2]
            end
        end
        local about = affine.scale(k)
        if tilt ~= 0 then
            about = affine.multiply(affine.rotate(tilt), about)
        end
        local own = view_at(x + w / 2)
        local transform = affine.chain(own, affine.translate(x, y), affine.about(pivot_x, pivot_y, about))
        entry.transform = transform
        entry.fit_x, entry.fit_y = fit_x, fit_y
        entry.contents = { transform[1] * fit_x, transform[2] * fit_y, transform[3] * fit_x, transform[4] * fit_y, transform[5], transform[6] }
        if entry.closing and not mo then
            hz.forget_closed(id)
            entries[id] = nil
            return
        end
        -- Out of sight: not drawn.
        local left, top = affine.apply(transform, 0, 0)
        local right, bottom = affine.apply(transform, w, h)
        local reach = 80
        entry.visible = right > sx - reach and left < sx + sw + reach and bottom > sy - reach and top < sy + sh + reach
        if not entry.visible or opacity <= 0 then
            return
        end
        if not entry.closing then
            table.insert(order, id)
        end
        -- A window that opens comes out of the fire glowing, and cools to
        -- what it shows; one that closes goes dark, the color of embers.
        local brightness = (s.cold_brightness + (1 - s.cold_brightness) * heat)
            * (1 - 0.3 * overview * (1 - chosen_of(entry.workspace or selected)))
        local glow = nil
        local fresh = math.exp(-math.max(t - entry.opened, 0) / 0.2)
        if entry.closing then
            local out = 1 - opacity
            brightness *= 1 - 0.7 * out
            glow = { colors.ember[1], colors.ember[2], colors.ember[3], 0.5 * out }
        elseif fresh > 0.004 then
            glow = { colors.hot[1], colors.hot[2], colors.hot[3], 0.75 * fresh }
        end
        table.insert(scene.windows, {
            id = id,
            over = entry.layer == "float" or (held ~= nil and held.id == id),
            transform = transform,
            width = w,
            height = h,
            fit_x = fit_x,
            fit_y = fit_y,
            radius = if entry.layer == "full" then 0 else s.radius,
            opacity = opacity,
            -- Colder without the focus, and in the overview on a workspace
            -- that is not the chosen one.
            brightness = brightness,
            glow = glow,
            look = window_look(entry, state, t, phase),
        })
    end
    local closing = {}
    for id, entry in entries do
        if entry.closing then
            table.insert(closing, id)
        end
    end
    table.sort(closing)
    local function add_workspace(index)
        -- Among the tiles, the one with the focus is drawn last: it is the
        -- one that passes over the others when it moves.
        local above = nil
        for place, id in space.windows_of(index) do
            local entry = entries[id]
            if entry and not (held and held.id == id) then
                local trail = s.trail + s.trail_step * (place - 1)
                if entry.layer == "tile" and entry.focused then
                    above = { id, entry, trail }
                else
                    if above and entry.layer ~= "tile" then
                        add(above[1], above[2], above[3])
                        above = nil
                    end
                    add(id, entry, trail)
                end
            end
        end
        if above then
            add(above[1], above[2], above[3])
        end
        for _, id in closing do
            local entry = entries[id]
            if entry and entry.workspace == index then
                add(id, entry, s.trail)
            end
        end
    end
    for index = 1, count do
        if index ~= selected then
            add_workspace(index)
        end
    end
    add_workspace(selected)
    if held and entries[held.id] then
        -- In hand: under the pointer, wherever the camera is.
        local entry = entries[held.id]
        local px, py = hz.pointer()
        local inverse = affine.invert(view)
        if px and inverse then
            local x, y = affine.apply(inverse, px, py)
            spring.snap(entry.x, x - held.dx)
            spring.snap(entry.y, y - held.dy)
            -- It leans the way it is carried, the more the faster.
            if held.seen and t > held.seen then
                local speed_x = (px - held.px) / (t - held.seen)
                held.speed = (held.speed or 0) * 0.6 + speed_x * 0.4
            end
            if not held.seen or t > held.seen then
                held.px, held.seen = px, t
            end
            local lean = math.clamp((held.speed or 0) / s.held_speed, -1, 1) * s.held_tilt
            spring.aim(entry.tilt, lean, t)
        end
        add(held.id, entry, 0)
    end

    -- The workspace marks: one for each workspace, the one in view wide and
    -- hot. Hidden under a fullscreen window.
    local covered = false
    for _, id in space.windows_of(selected) do
        if space.is_fullscreen(id) then
            covered = true
        end
    end
    if not covered or apart > 0.002 then
        local unit, gap, tall = 7, 9, 5
        local total = 0
        local widths = {}
        for index = 1, count do
            local near = math.max(0, 1 - math.abs(position - index))
            widths[index] = unit + 22 * near
            total += widths[index]
        end
        total += gap * (count - 1)
        local x = sx + (sw - total) / 2
        local y = sy + sh - 14 - tall / 2
        -- A dark tray under them, so that they read against the fire.
        table.insert(scene.marks, {
            transform = affine.translate(x - 12, y - 7),
            width = total + 24,
            height = tall + 14,
            look = {
                radius = (tall + 14) / 2,
                margin = 12,
                fill = colors.backing,
                fill_alpha = 0.62,
                frost = if s.frost then 1 else 0,
                frost_level = 2,
                rim = colors.cold,
                line = 0.22,
                line_width = 1,
                shadow = 0.3,
                softness = 5,
                drop = 1,
                opacity = 1,
            },
        })
        for index = 1, count do
            local near = math.max(0, 1 - math.abs(position - index))
            local used = #space.order_of(index) > 0
            local base = if used then 0.75 else 0.3
            table.insert(scene.marks, {
                transform = affine.translate(x, y),
                width = widths[index],
                height = tall,
                look = {
                    radius = tall / 2,
                    margin = 14,
                    fill = {
                        colors.mark[1] + (colors.hot[1] - colors.mark[1]) * near,
                        colors.mark[2] + (colors.hot[2] - colors.mark[2]) * near,
                        colors.mark[3] + (colors.hot[3] - colors.mark[3]) * near,
                    },
                    fill_alpha = base + (1 - base) * near,
                    glow_color = colors.hot,
                    glow = 0.5 * near,
                    glow_width = 4,
                    shadow = 0.35,
                    softness = 4,
                    drop = 1,
                    opacity = 1,
                },
            })
            x += widths[index] + gap
        end
    end

    -- Which windows are seen: those this frame drew, and those of the
    -- workspace the camera is at or on its way to (they wake up as the
    -- camera sets off, not only when they come into the picture). Every
    -- other window is told to stop drawing (`hz.set_visible`): a workspace
    -- out of view, a window flown off to one.
    local seen = {}
    for _, id in order do
        seen[id] = true
    end
    local now_told = {}
    for _, window in hz.windows() do
        local id = window.id
        local index = space.workspace_of(id)
        local visible = seen[id] == true or index == nil or index == selected or not s.hide_unseen
        if told[id] ~= visible and not pcall(hz.set_visible, id, visible) then
            visible = nil
        end
        now_told[id] = visible
    end
    told = now_told

    shown = {
        view = view,
        views = views,
        zoom = scale,
        camera = position,
        pull = pull,
        overview = overview,
        moving = moving,
        phase = phase,
        visible = visible,
        order = order,
    }
    if moving then
        hz.request_frame()
    end
    return scene
end

-- Events -----------------------------------------------------------------

function G.init()
    entries = {}
    told = {}
    clock = -math.huge
    held, handheld = nil, nil
    present_at_start = {}
    for name, c in G.settings.colors do
        colors[name] = paint.color(c[1], c[2], c[3])
    end
    camera = new_spring(space.active(), "camera", 0.0004)
    zoom = new_spring(0, "zoom", 0.0008)
    stir = { phase = 0, energy = 0, since = 0 }
    for _, window in hz.windows() do
        present_at_start[window.id] = true
        hz.keep_closed(window.id)
    end
end

function G.window_opened(id)
    hz.keep_closed(id)
    G.retarget()
end

-- Something may have changed: aim the springs now.
function G.retarget()
    if sync(now()) then
        hz.request_frame()
    end
end

-- The overview ------------------------------------------------------------

function G.in_overview()
    return zoom.target == 1
end

function G.set_overview(on)
    spring.aim(zoom, if on then 1 else 0, now())
    G.stir(0.6)
end

-- Input -------------------------------------------------------------------

-- The window drawn at a point of the screen (topmost first), and the point
-- in that window's coordinates.
function G.window_at(x, y)
    local order = shown.order
    for i = #order, 1, -1 do
        local id = order[i]
        local entry = entries[id]
        if entry and entry.visible and entry.transform and not entry.closing then
            local inverse = affine.invert(entry.transform)
            if inverse then
                local wx, wy = affine.apply(inverse, x, y)
                if wx >= 0 and wy >= 0 and wx < entry.drawn.w and wy < entry.drawn.h then
                    return id, wx, wy
                end
            end
        end
    end
    return nil
end

-- The workspace drawn at a point of the screen, or nil (the wall).
function G.workspace_at(x, y)
    local sx, sy, sw, sh = output_rect()
    for index, own in shown.views do
        local inverse = affine.invert(own)
        if inverse then
            local px, py = affine.apply(inverse, x, y)
            local along = px - sx - (index - 1) * pitch()
            if along >= 0 and along < sw and py >= sy and py < sy + sh then
                return index
            end
        end
    end
    return nil
end

-- Where a window's contents are drawn (the window's coordinates to the
-- screen), and how large the window is in its own coordinates: for hit
-- testing.
function G.contents_of(id)
    local entry = entries[id]
    if entry and entry.visible and not entry.closing then
        return entry.contents, entry.drawn.w / entry.fit_x, entry.drawn.h / entry.fit_y
    end
    return nil
end

-- The pointer takes hold of a window at a point of the screen.
function G.hold(id, x, y)
    local entry = entries[id]
    if not entry or not entry.drawn then
        return false
    end
    local t = now()
    local inverse = affine.invert(shown.view)
    local px, py = affine.apply(inverse, x, y)
    held = { id = id, dx = px - entry.drawn.x, dy = py - entry.drawn.y }
    entry.pivot = { held.dx, held.dy }
    spring.aim(entry.lift, 1, t)
    spring.aim(entry.scale, G.settings.held_scale, t)
    G.stir(0.4)
    return true
end

-- The pointer moved while holding a window (the next frame draws it
-- there).
function G.hold_to(x, y)
    if held then
        hz.request_frame()
    end
end

-- The tile (of the workspace in view) whose place is at a point of the
-- screen: where the tiles belong, not where they are drawn on their way.
function G.tile_at(x, y)
    local inverse = affine.invert(shown.view)
    if not inverse then
        return nil
    end
    local px, py = affine.apply(inverse, x, y)
    local index = space.active()
    px -= (index - 1) * pitch()
    for _, id in space.order_of(index) do
        local rect = space.rect_of(id)
        if rect and rect.layer == "tile" then
            if px >= rect.x and py >= rect.y and px < rect.x + rect.w and py < rect.y + rect.h then
                return id
            end
        end
    end
    return nil
end

-- The pointer lets go: the window goes to its tile.
function G.release()
    local entry = held and entries[held.id]
    held = nil
    if entry then
        local t = now()
        spring.aim(entry.lift, 0, t)
        spring.aim(entry.scale, 1, t)
        spring.aim(entry.tilt, 0, t)
    end
    G.retarget()
    hz.request_frame()
end

function G.held()
    return held and held.id
end

-- A camera position given by hand (a swipe), in workspaces; nil lets go,
-- with the speed it had (workspaces per second).
function G.carry(position, speed)
    local t = now()
    if position then
        handheld = position
        spring.snap(camera, position)
    elseif handheld then
        local from = handheld
        handheld = nil
        spring.snap(camera, space.active())
        spring.place(camera, from, t, speed or 0)
    end
    hz.request_frame()
end

-- Where the camera is now, in workspaces.
function G.camera_now()
    if handheld then
        return handheld
    end
    return (spring.at(camera, now()))
end

-- The windows the last frame drew, bottom to top.
function G.drawn_order()
    return shown.order
end

-- For tools and tests: what the last frame showed.
function G.describe()
    local list = {}
    for id, entry in entries do
        local d = entry.drawn
        if d then
            -- Where it is on screen.
            local x, y = affine.apply(entry.transform, 0, 0)
            local right, bottom = affine.apply(entry.transform, d.w, d.h)
            table.insert(list, {
                id = id,
                x = x,
                y = y,
                width = right - x,
                height = bottom - y,
                scale = d.scale,
                opacity = d.opacity,
                heat = d.heat,
                workspace = entry.workspace,
                visible = entry.visible == true,
                -- What the core was told (`hz.set_visible`).
                seen = told[id] ~= false,
                closing = entry.closing == true,
            })
        end
    end
    table.sort(list, function(a, b)
        return a.id < b.id
    end)
    return {
        camera = shown.camera,
        zoom = shown.zoom,
        pull = shown.pull,
        overview = shown.overview,
        moving = shown.moving,
        windows = list,
    }
end

-- Runs the work of a frame `count` times without drawing, for measuring.
function G.bench(count)
    local output = hz.outputs()[1]
    for _ = 1, count do
        G.frame(clock, output)
    end
end

return G
