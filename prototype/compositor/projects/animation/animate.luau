-- Window animations, computed from time.
--
-- The window manager says where a window *should* be: its place and size
-- (`hz.windows()`). This module decides where it is *drawn*. For every
-- window it keeps a tween: a start state, an end state, a start time and a
-- duration. The state drawn at time t is the start state blended towards
-- the end state by `easing((t - start) / duration)`.
--
--   - When the target of a window changes (a swap, a neighbour that opened
--     or closed, a new size from the client), a new tween starts from the
--     state the window is drawn in at that moment. A window that is moving
--     does not jump back: the new motion picks up where it is.
--   - A window that opens starts small and transparent; one that closes
--     ends small and transparent. The core keeps what a closed window last
--     showed because `window_opened` asked it to (`hz.keep_closed`); once
--     its tween is over, the window is let go (`hz.forget_closed`).
--   - Time is the time the frame is for (`frame`'s `time`: when it is
--     expected on screen), not the time the script runs. Events use
--     `hz.time()`, but never earlier than the last frame.
--   - While a tween runs, every frame asks for the next one
--     (`hz.request_frame()`). When nothing moves, nothing is drawn.
--
-- A window is drawn as a rectangle (x, y, w, h: its geometry on screen),
-- scaled by `scale` about its centre, with `opacity`. Until a client draws
-- at a new size, its old contents are stretched to the rectangle.

local affine = hz.include("affine")

local A = {}

-- Replaced by the project's settings (`main.luau`).
A.settings = {
    move_duration = 0.30,
    move_easing = "ease_out_cubic",
    open_duration = 0.25,
    open_easing = "ease_out_cubic",
    open_scale = 0.8,
    close_duration = 0.20,
    close_easing = "ease_in_out_cubic",
    close_scale = 0.8,
}

-- Easing functions: progress 0..1 in, blend 0..1 out.
A.easings = {
    linear = function(t)
        return t
    end,
    ease_out_cubic = function(t)
        local u = 1 - t
        return 1 - u * u * u
    end,
    ease_in_out_cubic = function(t)
        if t < 0.5 then
            return 4 * t * t * t
        end
        local u = 2 - 2 * t
        return 1 - u * u * u / 2
    end,
    ease_out_expo = function(t)
        return if t >= 1 then 1 else 1 - 2 ^ (-10 * t)
    end,
    -- Overshoots a little and settles back.
    ease_out_back = function(t)
        local c = 1.70158
        local u = t - 1
        return 1 + (c + 1) * u * u * u + c * u * u
    end,
}

local FIELDS = { "x", "y", "w", "h", "scale", "opacity" }

-- Window id -> {
--   tween = { from, to, start, duration, easing },  -- states are tables of FIELDS
--   now = the state drawn at the last frame,
--   size = { w, h } of the contents (what the client committed),
--   closing = true once the window is gone,
-- }
local windows = {}
-- The time animations run on: the latest frame time or event time seen.
local clock = -math.huge
-- Windows that were there when this script started: no open animation.
local present_at_start = {}

local function tick(t)
    if t > clock then
        clock = t
    end
    return clock
end

local function ease(name)
    return A.easings[name] or A.easings.linear
end

-- The state of a tween at time t, and whether it is still running.
local function sample(tween, t)
    local p = if tween.duration > 0 then (t - tween.start) / tween.duration else 1
    if p >= 1 then
        return tween.to, false
    end
    p = math.max(p, 0)
    local k = tween.easing(p)
    local state = {}
    for _, f in FIELDS do
        state[f] = tween.from[f] + (tween.to[f] - tween.from[f]) * k
    end
    return state, true
end

-- Starts a tween towards `to` from wherever the window is at time t.
local function start(entry, to, t, duration, easing)
    local from = if entry.tween then (sample(entry.tween, t)) else (entry.now or to)
    entry.tween = { from = from, to = to, start = t, duration = duration, easing = ease(easing) }
end

local function same_place(a, b)
    return a.x == b.x and a.y == b.y and a.w == b.w and a.h == b.h
end

-- Where the window manager wants a window, as a state.
local function target_of(window)
    return { x = window.x, y = window.y, w = window.width, h = window.height, scale = 1, opacity = 1 }
end

-- Compares every window with its target and starts the tweens that are
-- needed, at time t. Returns whether it started any.
local function sync(t)
    local s = A.settings
    local live = {}
    local started = false
    for _, window in hz.windows() do
        if window.mapped and window.width and window.height and window.width > 0 then
            live[window.id] = true
            local target = target_of(window)
            local entry = windows[window.id]
            if not entry then
                entry = {}
                windows[window.id] = entry
                if present_at_start[window.id] then
                    entry.now = target
                    entry.tween = { from = target, to = target, start = t, duration = 0, easing = ease("linear") }
                else
                    -- Opening: grow from the middle of where it will be.
                    local from = table.clone(target)
                    from.scale = s.open_scale
                    from.opacity = 0
                    entry.now = from
                    start(entry, target, t, s.open_duration, s.open_easing)
                    started = true
                end
            elseif not same_place(entry.tween.to, target) then
                -- Somewhere else now: from where it is drawn, to there. A
                -- window still opening keeps growing and fading in on the way.
                start(entry, target, t, s.move_duration, s.move_easing)
                started = true
            end
            entry.size = { w = window.width, h = window.height }
        end
    end
    for _, closed in hz.closed_windows() do
        live[closed.id] = true
        local entry = windows[closed.id]
        if not entry or not entry.now then
            -- Never drawn: nothing to fade out.
            hz.forget_closed(closed.id)
            windows[closed.id] = nil
        elseif not entry.closing then
            entry.closing = true
            local to = table.clone((sample(entry.tween, t)))
            to.scale = s.close_scale * to.scale
            to.opacity = 0
            start(entry, to, t, s.close_duration, s.close_easing)
            started = true
        end
    end
    for id in windows do
        if not live[id] then
            windows[id] = nil
        end
    end
    return started
end

-- The transform from a window's own coordinates (its contents, 0..size)
-- to the screen, for a drawn state.
local function transform(state, size)
    local cx, cy = state.x + state.w / 2, state.y + state.h / 2
    return affine.chain(
        affine.about(cx, cy, affine.scale(state.scale)),
        affine.translate(state.x, state.y),
        affine.scale(state.w / math.max(size.w, 1), state.h / math.max(size.h, 1))
    )
end

-- Moves every tween to time t: what to draw, bottom to top, for
-- `render.frame`. Closed windows go on top.
local function step(t)
    local moving = false
    local items = {}
    local closing = {}
    for id, entry in windows do
        local state, running = sample(entry.tween, t)
        entry.now = state
        entry.transform = transform(state, entry.size)
        if running then
            moving = true
        elseif entry.closing then
            hz.forget_closed(id)
            windows[id] = nil
        end
    end
    for _, window in hz.windows() do
        local entry = windows[window.id]
        if entry then
            table.insert(items, { id = window.id, transform = entry.transform, opacity = entry.now.opacity })
        end
    end
    for _, closed in hz.closed_windows() do
        local entry = windows[closed.id]
        if entry then
            table.insert(closing, { id = closed.id, transform = entry.transform, opacity = entry.now.opacity })
        end
    end
    for _, item in closing do
        table.insert(items, item)
    end
    return items, moving
end

function A.init()
    windows = {}
    clock = -math.huge
    present_at_start = {}
    for _, window in hz.windows() do
        present_at_start[window.id] = true
        hz.keep_closed(window.id)
    end
end

-- A window was created. Its contents are to be kept when it closes, for the
-- close animation.
function A.window_opened(id)
    hz.keep_closed(id)
    A.retarget()
end

-- Something may have moved: start the tweens now.
function A.retarget()
    if sync(tick(hz.time())) then
        hz.request_frame()
    end
end

-- The work of a frame at time t. Returns what to draw.
function A.frame(t)
    t = tick(t)
    sync(t)
    local items, moving = step(t)
    if moving then
        hz.request_frame()
    end
    return items
end

-- Where a window is drawn, for hit testing: the transform of the last frame,
-- which is what is on screen.
function A.transform_of(window)
    local entry = windows[window.id]
    if entry and entry.transform then
        return entry.transform
    end
    return affine.translate(window.x, window.y)
end

function A.describe()
    local list = {}
    for id, entry in windows do
        if entry.now then
            table.insert(list, {
                id = id,
                x = entry.now.x,
                y = entry.now.y,
                width = entry.now.w,
                height = entry.now.h,
                scale = entry.now.scale,
                opacity = entry.now.opacity,
                closing = entry.closing == true,
                moving = clock < entry.tween.start + entry.tween.duration,
            })
        end
    end
    table.sort(list, function(a, b)
        return a.id < b.id
    end)
    return list
end

-- Runs what `A.frame` does, without drawing, `count` times at the current
-- time. Nothing changes from one round to the next.
function A.bench(count)
    for _ = 1, count do
        sync(clock)
        step(clock)
    end
end

return A
