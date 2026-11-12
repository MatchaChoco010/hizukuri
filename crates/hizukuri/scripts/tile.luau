-- A simple tiling window manager.
--
-- Windows stand side by side and share the first output evenly. The order
-- is this script's own state; the core is told the resulting position and
-- size of each window, and which one has the focus.
--
-- Keys (with the logo key held):
--   L / H          focus the window to the right / left
--   Shift+L / H    swap the focused window with its right / left neighbour
--   Q              ask the focused window to close

local T = {}

local KEY_Q, KEY_H, KEY_L = 16, 35, 38

-- Window ids, left to right.
local order = {}
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

-- Recomputes the position and size of every window.
function T.arrange()
    local output = hz.outputs()[1]
    local count = #order
    if not output or count == 0 then
        return
    end
    local gap = T.gap
    local width = (output.logical_width - gap * (count + 1)) // count
    local height = output.logical_height - gap * 2
    for i, id in order do
        hz.place(id, output.x + gap + (i - 1) * (width + gap), output.y + gap)
        hz.configure(id, width, height)
    end
    hz.restack(order)
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
    for _, window in hz.windows() do
        if not known[window.id] then
            table.insert(order, window.id)
        end
    end
    T.arrange()
end

function T.save()
    return { order = order }
end

function T.order()
    return order
end

function T.window_opened(id)
    if not index_of(id) then
        table.insert(order, id)
    end
    T.arrange()
    hz.focus(id)
end

function T.window_closed(id)
    local index = index_of(id)
    if not index then
        return
    end
    table.remove(order, index)
    T.arrange()
    -- The neighbour that moved into the gap gets the focus.
    local next_id = order[math.min(index, #order)]
    hz.focus(next_id)
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
    if not mods.logo then
        return false
    end
    if keycode ~= KEY_H and keycode ~= KEY_L and keycode ~= KEY_Q then
        return false
    end
    if pressed then
        if keycode == KEY_L then
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

return T
