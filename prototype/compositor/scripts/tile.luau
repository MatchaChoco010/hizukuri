-- A simple tiling window manager.
--
-- Windows stand side by side and share the first output evenly. The order
-- is this script's own state; the core is told the resulting position and
-- size of each window, and which one has the focus.
--
-- Some windows are not tiled:
--   - a window that belongs to another one (a dialog: it has a parent), or
--     whose client fixed its size (minimum and maximum are the same), floats
--     above the tiles at the size its client chose, centered over its
--     parent (or the output);
--   - a window whose client asked to be fullscreen covers the output, above
--     everything else, until it asks to stop.
-- A client's request to activate one of its windows (xdg-activation) moves
-- the focus there when it came from the user's input.
--
-- Keys (with the logo key held):
--   L / H          focus the window to the right / left
--   Shift+L / H    swap the focused window with its right / left neighbour
--   Q              ask the focused window to close
--   Return         start a terminal: the command in the environment variable
--                  HIZUKURI_TERMINAL (words separated by spaces), or `foot`
-- While the focused window asks for the compositor's shortcuts
-- (keyboard-shortcuts-inhibit), none of these keys is taken.
--   Escape         free the pointer from a lock or confinement (a game that
--                  holds the pointer); it is not held again until the
--                  focus moves or the pointer comes back onto the window
--
-- Touchpad: three fingers swiped right or left focus the tile to the right
-- or left (floating and fullscreen windows are passed over; from a dialog
-- the move starts at its parent). Those swipes are this script's:
-- applications do not see them. Every other gesture (a two finger pinch, a four finger swipe)
-- goes to the application under the pointer.

local T = {}

local KEY_ESC, KEY_Q, KEY_H, KEY_L, KEY_ENTER = 1, 16, 35, 38, 28

-- The command a terminal is started with.
local function terminal()
    local argv = {}
    for word in string.gmatch(hz.env("HIZUKURI_TERMINAL") or "foot", "%S+") do
        table.insert(argv, word)
    end
    return argv
end

-- Window ids, left to right.
local order = {}
-- Windows that asked to be fullscreen: id -> true.
local fullscreen = {}
-- What each window was last arranged as (see `T.window_changed`).
local seen = {}
T.gap = 8

local function index_of(id)
    for i, other in order do
        if other == id then
            return i
        end
    end
    return nil
end

local function focused()
    for _, window in hz.windows() do
        if window.focused then
            return window.id
        end
    end
    return nil
end

-- Whether a window stays out of the tiles (see the top of this file).
local function floats(window)
    if window.parent ~= nil then
        return true
    end
    return window.min_width > 0
        and window.min_width == window.max_width
        and window.min_height > 0
        and window.min_height == window.max_height
end

-- Recomputes the position and size of every window.
function T.arrange()
    local output = hz.outputs()[1]
    if not output or #order == 0 then
        return
    end
    local windows = {}
    for _, window in hz.windows() do
        windows[window.id] = window
    end
    local tiled, floating, full = {}, {}, {}
    for _, id in order do
        local window = windows[id]
        if fullscreen[id] then
            table.insert(full, id)
        elseif window and floats(window) then
            table.insert(floating, id)
        else
            table.insert(tiled, id)
        end
    end
    local gap = T.gap
    local count = #tiled
    if count > 0 then
        local width = (output.logical_width - gap * (count + 1)) // count
        local height = output.logical_height - gap * 2
        for i, id in tiled do
            hz.place(id, output.x + gap + (i - 1) * (width + gap), output.y + gap)
            hz.configure(id, width, height)
        end
    end
    for _, id in floating do
        local window = windows[id]
        local over = window.parent and windows[window.parent]
        local x, y, width, height = output.x, output.y, output.logical_width, output.logical_height
        if over and over.width then
            x, y, width, height = over.x, over.y, over.width, over.height
        end
        hz.configure(id, nil, nil)
        hz.place(id, x + (width - (window.width or 0)) // 2, y + (height - (window.height or 0)) // 2)
    end
    for _, id in full do
        hz.place(id, output.x, output.y)
        hz.configure(id, output.logical_width, output.logical_height)
    end
    local stack = {}
    for _, list in { tiled, floating, full } do
        for _, id in list do
            table.insert(stack, id)
        end
    end
    hz.restack(stack)
    hz.request_frame()
end

function T.init(saved)
    -- Windows that are already open (after a reload) keep their order.
    local known = {}
    if saved and saved.order then
        for _, id in saved.order do
            known[id] = true
        end
    end
    local present = {}
    for _, window in hz.windows() do
        present[window.id] = true
    end
    order = {}
    if saved and saved.order then
        for _, id in saved.order do
            if present[id] then
                table.insert(order, id)
            end
        end
    end
    -- The others (all of them when nothing was handed over, as after a
    -- reload while the script was out) follow, left to right as they are
    -- on screen.
    local others = {}
    for _, window in hz.windows() do
        if not known[window.id] then
            table.insert(others, window)
        end
    end
    table.sort(others, function(a, b)
        if a.x ~= b.x then
            return a.x < b.x
        end
        if a.y ~= b.y then
            return a.y < b.y
        end
        return a.id < b.id
    end)
    for _, window in others do
        table.insert(order, window.id)
    end
    fullscreen = {}
    if saved and saved.fullscreen then
        for _, id in saved.fullscreen do
            if present[id] then
                fullscreen[id] = true
            end
        end
    end
    T.arrange()
end

function T.save()
    local full = {}
    for id in fullscreen do
        table.insert(full, id)
    end
    return { order = order, fullscreen = full }
end

function T.order()
    return order
end

function T.window_opened(id)
    if not index_of(id) then
        table.insert(order, id)
    end
    -- A tiled window has its place already; a floating one is placed again
    -- once its size is known (`window_changed`).
    seen[id] = "tiled"
    for _, window in hz.windows() do
        if window.id == id and floats(window) then
            seen[id] = nil
        end
    end
    T.arrange()
    hz.focus(id)
end

function T.window_closed(id)
    local index = index_of(id)
    if not index then
        return
    end
    fullscreen[id] = nil
    seen[id] = nil
    table.remove(order, index)
    T.arrange()
    -- The neighbour that moved into the gap gets the focus.
    local next_id = order[math.min(index, #order)]
    hz.focus(next_id)
end

-- A window of this script that floats is centered again when its size
-- changes, and one whose parent or size limits changed may move in or out
-- of the tiles. `seen` keeps what the arrangement was made for.
function T.window_changed(id)
    if not index_of(id) then
        return
    end
    for _, window in hz.windows() do
        if window.id == id then
            local now = "tiled"
            if floats(window) then
                now = string.format("floats %dx%d", window.width or 0, window.height or 0)
            end
            if seen[id] ~= now then
                seen[id] = now
                T.arrange()
            end
            return
        end
    end
end

-- What a client asks of the window manager (`kind`: fullscreen,
-- unfullscreen, activate, maximize, move, ...).
function T.window_request(id, request)
    if request.kind == "fullscreen" then
        fullscreen[id] = true
        hz.set_states(id, { fullscreen = true })
        T.arrange()
        hz.focus(id)
    elseif request.kind == "unfullscreen" then
        fullscreen[id] = nil
        hz.set_states(id, { fullscreen = false })
        T.arrange()
    elseif request.kind == "activate" then
        if request.from_input then
            hz.focus(id)
        end
    end
end

function T.outputs_changed()
    T.arrange()
end

local function step(direction, swap)
    local current = focused()
    local index = current and index_of(current)
    if not index then
        return
    end
    local target = index + direction
    if target < 1 or target > #order then
        return
    end
    if swap then
        order[index], order[target] = order[target], order[index]
        T.arrange()
    else
        hz.focus(order[target])
    end
end

-- Returns true for keys this window manager uses, so that they do not reach
-- a client.
function T.key(keycode, pressed, mods)
    -- A window that asked for the compositor's shortcuts (a nested
    -- compositor, a remote desktop) gets them while it has the focus.
    if not mods.logo or mods.inhibited then
        return false
    end
    if
        keycode ~= KEY_H
        and keycode ~= KEY_L
        and keycode ~= KEY_Q
        and keycode ~= KEY_ESC
        and keycode ~= KEY_ENTER
    then
        return false
    end
    if pressed then
        if keycode == KEY_ENTER then
            local _, problem = hz.spawn(terminal())
            if problem then
                hz.log("could not start a terminal: " .. problem)
            end
        elseif keycode == KEY_ESC then
            hz.release_pointer_constraint()
        elseif keycode == KEY_L then
            step(1, mods.shift)
        elseif keycode == KEY_H then
            step(-1, mods.shift)
        elseif keycode == KEY_Q then
            local current = focused()
            if current then
                hz.close(current)
            end
        end
    end
    return true
end

-- How far (logical pixels) three fingers have to travel sideways to move
-- the focus.
T.swipe_distance = 60
-- The horizontal travel of the swipe this script took, or nil.
local swipe = nil

-- Takes three finger swipes; leaves every other gesture to the
-- application.
function T.gesture_begin(g)
    if g.kind == "swipe" and g.fingers == 3 then
        swipe = 0
        return true
    end
    return false
end

function T.gesture_update(g)
    if swipe and g.kind == "swipe" then
        swipe += g.dx
    end
end

-- Moves the focus to the next tile to the right (1) or left (-1). Only the
-- tiles count: floating windows (dialogs) and fullscreen ones are passed
-- over. From a floating window, the move starts at its parent's tile.
local function step_tiles(direction)
    local windows = {}
    for _, window in hz.windows() do
        windows[window.id] = window
    end
    local tiles = {}
    for _, id in order do
        local window = windows[id]
        if window and not fullscreen[id] and not floats(window) then
            table.insert(tiles, id)
        end
    end
    local current = focused()
    if current and windows[current] and windows[current].parent then
        current = windows[current].parent
    end
    for i, id in tiles do
        if id == current then
            local target = tiles[i + direction]
            if target then
                hz.focus(target)
            end
            return
        end
    end
end

function T.gesture_end(g)
    local travel = swipe
    swipe = nil
    if not travel or g.cancelled then
        return
    end
    if travel >= T.swipe_distance then
        step_tiles(1)
    elseif travel <= -T.swipe_distance then
        step_tiles(-1)
    end
end

return T
