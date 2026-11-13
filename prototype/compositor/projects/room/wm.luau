-- Which window is on which monitor.
--
-- Every monitor of the room shows at most one window. The other windows
-- are open, but on no monitor: they wait until one is chosen.
--
--   - One monitor has the focus. Its window has the keyboard.
--   - A new window goes to a monitor without a signal when there is one
--     (the focused one first), else onto the focused monitor, whose window
--     then waits. The focus follows the new window.
--   - Tab (with the logo key) puts the next waiting window on the focused
--     monitor; with shift, the previous one.
--   - When the window of a monitor closes, the next waiting window takes
--     its place. With none waiting, the monitor has no signal.
--   - A window is asked to be as large as the resolution of its monitor.
--   - A window that belongs to another one (a dialog) is shown on the
--     monitor of its parent, over it, at its own size.
--   - The popups of a window (menus, tooltips) are kept on its monitor
--     (`hz.set_popup_bounds`), not on the output.
--
-- A change of what a monitor shows is recorded as a transition (what it
-- showed before, since when, in which direction), which the renderer
-- plays.

local W = {}

W.settings = {
    -- Seconds a monitor takes to change what it shows.
    switch_duration = 0.4,
    -- The program logo+Return starts, as a list of arguments. Without it:
    -- what the environment variable HIZUKURI_TERMINAL names (split at
    -- spaces), else `foot`.
    terminal = nil,
}

-- The command line of the terminal.
function W.terminal()
    if W.settings.terminal then
        return W.settings.terminal
    end
    local argv = {}
    for word in string.gmatch(hz.env("HIZUKURI_TERMINAL") or "foot", "%S+") do
        table.insert(argv, word)
    end
    return argv
end

local KEY_ESC, KEY_1, KEY_4, KEY_TAB, KEY_Q, KEY_F, KEY_H, KEY_J, KEY_K, KEY_L = 1, 2, 5, 15, 16, 33, 35, 36, 37, 38
local KEY_ENTER, KEY_UP, KEY_LEFT, KEY_RIGHT, KEY_DOWN = 28, 103, 105, 106, 108

local DIRECTIONS = {
    [KEY_H] = { -1, 0 },
    [KEY_LEFT] = { -1, 0 },
    [KEY_L] = { 1, 0 },
    [KEY_RIGHT] = { 1, 0 },
    [KEY_K] = { 0, 1 },
    [KEY_UP] = { 0, 1 },
    [KEY_J] = { 0, -1 },
    [KEY_DOWN] = { 0, -1 },
}

-- The monitors of the scene (set by `W.init`).
local monitors = {}
-- Monitor index -> { window = id or nil, transition = { from, start, direction } or nil }.
local slots = {}
-- Every window that takes a monitor of its own (no dialogs), in the order
-- Tab walks them.
local order = {}
-- The index of the monitor with the focus, and whether the camera is up
-- close on it.
local focused = 1
local zoomed = false
-- Called after anything changed that the camera or the picture depends on.
W.changed = function(_t) end
-- The time of the latest frame: events never happen before it.
local clock = -math.huge

function W.tick(t)
    if t > clock then
        clock = t
    end
    return clock
end

local function now()
    return W.tick(hz.time())
end

local function index_of(list, value)
    for i, other in list do
        if other == value then
            return i
        end
    end
    return nil
end

local function windows_by_id()
    local map = {}
    for _, window in hz.windows() do
        map[window.id] = window
    end
    return map
end

-- The window a dialog finally belongs to: the one that takes a monitor.
local function root_of(id, map)
    local seen = 0
    while map[id] and map[id].parent and map[map[id].parent] and seen < 16 do
        id = map[id].parent
        seen += 1
    end
    return id
end

-- The monitor a window is on, or nil while it waits.
function W.monitor_of(id)
    local root = root_of(id, windows_by_id())
    for index, slot in slots do
        if slot.window == root then
            return index
        end
    end
    return nil
end

-- The windows that wait, in the order Tab walks them.
local function waiting()
    local shown = {}
    for _, slot in slots do
        if slot.window then
            shown[slot.window] = true
        end
    end
    local list = {}
    for _, id in order do
        if not shown[id] then
            table.insert(list, id)
        end
    end
    return list
end

-- The window that has the keyboard when monitor `index` has the focus:
-- its newest dialog, else its window.
local function keyboard_target(index)
    local slot = slots[index]
    if not slot or not slot.window then
        return nil
    end
    local map = windows_by_id()
    local target = slot.window
    for _, window in hz.windows() do
        if window.id ~= slot.window and window.parent and root_of(window.id, map) == slot.window then
            target = window.id
        end
    end
    return target
end

-- A window this script still knows may be gone for the core already: when
-- several close at the same moment, the core has dropped them all before
-- it tells about the first. The core refuses a window it does not have;
-- these calls take that as an answer.

local function apply_focus()
    local target = keyboard_target(focused)
    if not target or not pcall(hz.focus, target) then
        hz.focus(nil)
    end
end

-- Keeps the popups (menus, tooltips) of the window `root` and of its
-- dialogs on monitor `index`: the part of each window's coordinates that
-- the monitor shows (the monitor's rectangle through the inverse of
-- `W.fit`). The core would keep them inside the output, as if the window
-- were drawn flat at its place.
local function bound_popups(index, root)
    local monitor = monitors[index]
    if not monitor then
        return
    end
    local map = windows_by_id()
    for _, window in hz.windows() do
        if window.width and root_of(window.id, map) == root then
            local t = W.fit(monitor, window.width, window.height)
            local x0, y0 = math.ceil(-t[5] / t[1]), math.ceil(-t[6] / t[4])
            local x1 = math.floor((monitor.pixels_x - t[5]) / t[1])
            local y1 = math.floor((monitor.pixels_y - t[6]) / t[4])
            pcall(hz.set_popup_bounds, window.id, { x = x0, y = y0, width = x1 - x0, height = y1 - y0 })
        end
    end
end

-- Asks a window to fill a monitor: the size of its resolution, and told
-- that it is maximized (a terminal then fills the size it is given,
-- instead of rounding it down to whole characters). Returns false when the
-- window is gone.
local function fill(index, window)
    local monitor = monitors[index]
    if not pcall(hz.place, window, 0, 0) then
        return false
    end
    pcall(hz.configure, window, monitor.pixels_x, monitor.pixels_y)
    pcall(hz.set_states, window, { maximized = true })
    bound_popups(index, window)
    return true
end

-- Shows `window` (or nothing) on a monitor, as a transition from what it
-- showed.
local function show(index, window, direction)
    local slot = slots[index]
    if slot.window == window then
        return
    end
    if window and not fill(index, window) then
        window = nil
    end
    slot.transition = { from = slot.window, start = now(), direction = direction or 1 }
    slot.window = window
end

-- Which monitor had the focus before, and since when the focused one has
-- it: its lamp comes on, the other's goes out.
local focus_before = nil
local focus_since = -math.huge
local LAMP_SECONDS = 0.3

local function set_focused(index)
    if index and monitors[index] and index ~= focused then
        focus_before = focused
        focus_since = now()
        focused = index
    end
end

local function focus_monitor(index)
    set_focused(index)
    apply_focus()
end

-- How much a monitor has the focus at time t (0..1), and whether that is
-- still changing.
function W.focus_of(index, t)
    local p = math.clamp((t - focus_since) / LAMP_SECONDS, 0, 1)
    local k = p * p * (3 - 2 * p)
    if index == focused then
        return k, p < 1
    elseif index == focus_before then
        return 1 - k, p < 1
    end
    return 0, false
end

function W.init(scene_monitors, saved)
    monitors = scene_monitors
    slots = {}
    for index in monitors do
        slots[index] = {}
    end
    order = {}
    focused = 1
    focus_before = nil
    focus_since = -math.huge
    zoomed = false
    local map = windows_by_id()
    -- Windows that are already open (after a reload) keep their monitors.
    if saved then
        for _, id in saved.order or {} do
            if map[id] and not index_of(order, id) then
                table.insert(order, id)
            end
        end
        for index, id in saved.shown or {} do
            if slots[index] and id and index_of(order, id) then
                slots[index].window = id
            end
        end
        if saved.focused and monitors[saved.focused] then
            focused = saved.focused
        end
        zoomed = saved.zoomed == true
    end
    for _, window in hz.windows() do
        if root_of(window.id, map) == window.id and not index_of(order, window.id) then
            W.window_opened(window.id, true)
        end
    end
    for index, slot in slots do
        slot.transition = nil
        if slot.window then
            fill(index, slot.window)
        end
    end
    for _, window in hz.windows() do
        hz.keep_closed(window.id)
    end
    apply_focus()
end

function W.save()
    local shown = {}
    for index, slot in slots do
        shown[index] = slot.window or false
    end
    return { order = order, shown = shown, focused = focused, zoomed = zoomed }
end

function W.window_opened(id, quiet)
    pcall(hz.keep_closed, id)
    local map = windows_by_id()
    if root_of(id, map) ~= id then
        -- A dialog: on its parent's monitor, at the size it chooses.
        pcall(hz.configure, id, nil, nil)
        pcall(hz.place, id, 0, 0)
        local index = W.monitor_of(id)
        if index then
            bound_popups(index, root_of(id, map))
        end
        apply_focus()
        W.changed(now())
        return
    end
    if index_of(order, id) then
        return
    end
    table.insert(order, id)
    -- A monitor without a signal: the focused one first.
    local target = nil
    if not slots[focused].window then
        target = focused
    else
        for index, slot in slots do
            if not slot.window then
                target = index
                break
            end
        end
    end
    target = target or focused
    show(target, id, 1)
    set_focused(target)
    if not quiet then
        apply_focus()
        W.changed(now())
    end
end

function W.window_closed(id)
    local at = index_of(order, id)
    if at then
        table.remove(order, at)
        for index, slot in slots do
            if slot.window == id then
                -- The next waiting window takes the monitor (the first
                -- one that is still there).
                slot.window = nil
                slot.transition = { from = id, start = now(), direction = -1 }
                for _, candidate in waiting() do
                    if fill(index, candidate) then
                        slot.window = candidate
                        break
                    end
                end
            end
        end
    end
    apply_focus()
    W.changed(now())
end

function W.window_changed(id)
    -- A new size moves the window on its monitor (`W.fit`).
    local index = W.monitor_of(id)
    if index then
        bound_popups(index, slots[index].window)
    end
    W.changed(now())
end

-- What a client asks of the window manager.
function W.window_request(id, request)
    if request.kind == "fullscreen" then
        -- A window already fills its monitor.
        pcall(hz.set_states, id, { maximized = true, fullscreen = true })
    elseif request.kind == "unfullscreen" then
        pcall(hz.set_states, id, { maximized = true, fullscreen = false })
    elseif request.kind == "activate" and request.from_input then
        local root = root_of(id, windows_by_id())
        local index = W.monitor_of(root)
        if not index and index_of(order, root) then
            show(focused, root, 1)
            index = focused
        end
        focus_monitor(index)
        W.changed(now())
    end
end

-- Puts the next (direction 1) or previous (-1) waiting window on the
-- focused monitor.
local function cycle(direction)
    local list = waiting()
    if #list == 0 then
        return
    end
    local current = slots[focused].window
    local chosen
    if not current then
        chosen = if direction > 0 then list[1] else list[#list]
    else
        -- The waiting window that follows the current one in the order.
        local at = index_of(order, current)
        local count = #order
        for step = 1, count - 1 do
            local candidate = order[(at - 1 + step * direction) % count + 1]
            if index_of(list, candidate) then
                chosen = candidate
                break
            end
        end
    end
    if chosen then
        show(focused, chosen, direction)
        apply_focus()
    end
end

-- The monitor next to the focused one in a direction (x to the right, y
-- up), or nil.
local function neighbour(direction)
    local from = monitors[focused].center
    local best, best_score = nil, math.huge
    for index, monitor in monitors do
        if index ~= focused then
            local dx, dy = monitor.center[1] - from[1], monitor.center[2] - from[2]
            local along = dx * direction[1] + dy * direction[2]
            local across = math.abs(dx * direction[2] - dy * direction[1])
            if along > 0.05 then
                local score = along + 2 * across
                if score < best_score then
                    best, best_score = index, score
                end
            end
        end
    end
    return best
end

-- Returns true for the keys this window manager uses.
function W.key(keycode, pressed, mods)
    if not mods.logo then
        return false
    end
    local direction = DIRECTIONS[keycode]
    local number = keycode >= KEY_1 and keycode <= KEY_4 and keycode - KEY_1 + 1 or nil
    if number and not monitors[number] then
        number = nil
    end
    local known = direction
        or number
        or keycode == KEY_TAB
        or keycode == KEY_F
        or keycode == KEY_Q
        or keycode == KEY_ENTER
        or keycode == KEY_ESC
    if not known then
        return false
    end
    if not pressed then
        return true
    end
    if direction then
        local target = neighbour(direction)
        if target and mods.shift then
            -- The two monitors trade what they show.
            local a, b = slots[focused].window, slots[target].window
            local sign = if direction[1] ~= 0 then direction[1] else direction[2]
            slots[focused].window = nil
            slots[target].window = nil
            slots[focused].transition = { from = a, start = now(), direction = sign }
            slots[target].transition = { from = b, start = now(), direction = sign }
            for index, id in { [focused] = b, [target] = a } do
                slots[index].window = id
                if id and not fill(index, id) then
                    slots[index].window = nil
                end
            end
            focus_monitor(target)
        elseif target then
            focus_monitor(target)
        end
    elseif number then
        focus_monitor(number)
    elseif keycode == KEY_TAB then
        cycle(if mods.shift then -1 else 1)
    elseif keycode == KEY_F then
        zoomed = not zoomed
    elseif keycode == KEY_Q then
        local target = keyboard_target(focused)
        if target then
            pcall(hz.close, target)
        end
    elseif keycode == KEY_ENTER then
        local pid, problem = hz.spawn(W.terminal())
        if not pid then
            hz.log("room: the terminal did not start: " .. tostring(problem))
        end
    elseif keycode == KEY_ESC then
        if hz.pointer_constraint() then
            hz.release_pointer_constraint()
        else
            zoomed = false
        end
    end
    W.changed(now())
    return true
end

-- A click on a monitor gives it the focus. Returns true: the focus is
-- this script's to give.
function W.pointer_button(_button, pressed, window, _surface)
    if pressed and window then
        local index = W.monitor_of(window)
        if index then
            if index ~= focused then
                focus_monitor(index)
                W.changed(now())
            else
                apply_focus()
            end
        end
    end
    return true
end

-- ----- for the renderer -------------------------------------------------

function W.focused()
    return focused
end

function W.zoomed()
    return zoomed
end

function W.slot(index)
    return slots[index]
end

-- Where a window's own coordinates lie on a monitor, in the monitor's
-- pixels: { a, b, c, d, tx, ty }. A window as large as the monitor maps
-- one to one; a smaller one is centered, a larger one is shrunk to fit.
function W.fit(monitor, width, height)
    local scale = math.min(1, monitor.pixels_x / math.max(width, 1), monitor.pixels_y / math.max(height, 1))
    return {
        scale,
        0,
        0,
        scale,
        (monitor.pixels_x - width * scale) / 2,
        (monitor.pixels_y - height * scale) / 2,
    }
end

-- What a monitor draws for the window `root`, bottom to top: the window,
-- then its dialogs. Each is { id, transform }. `root` may be a closed
-- window that is kept.
function W.items(monitor, root)
    local items = {}
    local map = windows_by_id()
    local window = map[root]
    if not window then
        for _, closed in hz.closed_windows() do
            if closed.id == root and closed.width then
                table.insert(items, { id = root, transform = W.fit(monitor, closed.width, closed.height) })
            end
        end
        return items
    end
    if window.width then
        table.insert(items, { id = root, transform = W.fit(monitor, window.width, window.height) })
    end
    for _, other in hz.windows() do
        if other.id ~= root and other.parent and other.width and root_of(other.id, map) == root then
            table.insert(items, { id = other.id, transform = W.fit(monitor, other.width, other.height) })
        end
    end
    return items
end

-- Ends the transitions that are over at time t and lets go of closed
-- windows that no monitor is showing the last of. Returns whether any
-- transition still runs.
function W.step(t)
    local running = false
    local needed = {}
    for _, slot in slots do
        local transition = slot.transition
        if transition then
            if t >= transition.start + W.settings.switch_duration then
                slot.transition = nil
            else
                running = true
                if transition.from then
                    needed[transition.from] = true
                end
            end
        end
    end
    for _, closed in hz.closed_windows() do
        if not needed[closed.id] then
            hz.forget_closed(closed.id)
        end
    end
    return running
end

-- The windows that are seen: what the monitors show (with the dialogs
-- over it), and what a monitor that changes its picture is showing on its
-- way out. With `only_focused` (the camera is up close on the focused
-- monitor, and still), only that monitor's. A window that waits for a
-- monitor is not seen.
function W.seen(only_focused)
    local seen = {}
    for index, slot in slots do
        if not only_focused or index == focused then
            if slot.window then
                seen[slot.window] = true
            end
            if slot.transition and slot.transition.from then
                seen[slot.transition.from] = true
            end
        end
    end
    local map = windows_by_id()
    for _, window in hz.windows() do
        if window.parent and seen[root_of(window.id, map)] then
            seen[window.id] = true
        end
    end
    return seen
end

-- For the tests and tools: what every monitor shows.
function W.describe()
    local list = {}
    local wait = waiting()
    for index, monitor in monitors do
        local slot = slots[index]
        table.insert(list, {
            index = index,
            name = monitor.name,
            window = slot.window or false,
            signal = slot.window ~= nil,
            focused = index == focused,
            switching = slot.transition ~= nil,
            width = monitor.pixels_x,
            height = monitor.pixels_y,
        })
    end
    local hidden = {}
    for _, window in hz.windows() do
        if not window.visible then
            table.insert(hidden, window.id)
        end
    end
    table.sort(hidden)
    return { monitors = list, waiting = wait, focused = focused, zoomed = zoomed, hidden = hidden }
end

return W
