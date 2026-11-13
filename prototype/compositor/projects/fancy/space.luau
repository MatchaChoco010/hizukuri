-- Workspaces and tiling: which window is where, and who has the focus.
--
-- This module decides; it draws nothing. The core is told the place and
-- size of every window (`hz.place`, `hz.configure`), the stacking order and
-- the focus. `stage` reads the same decisions (`P.rect_of`, `P.workspace_of`)
-- and moves what is drawn towards them.
--
-- Workspaces exist only here. To the core, every window of every workspace
-- is an ordinary window on the one output, at the place its tile has on its
-- own workspace: a window that is not on the workspace in view is simply
-- not drawn and not hit (`stage` and `main`), and the core is told that it
-- is not seen (`stage`, `hz.set_visible`).
--
-- A workspace keeps its windows in one order. The first is the main window
-- and takes the left part; the others share the right part, top to bottom
-- (`tall`). The other layout stands them side by side (`columns`).
-- Windows that belong to another window (dialogs) or whose client fixed
-- their size float above the tiles at the size the client chose; a window
-- that is fullscreen covers the output.

local P = {}

P.settings = {
    workspaces = 5,
    -- Logical pixels between tiles, and between the tiles and the edges.
    gap = 18,
    -- Room left free below the tiles, for the workspace marks.
    bottom = 22,
    -- The share of the width the main window takes.
    ratio = 0.56,
}

-- Workspace index -> { order = { ids }, focus = id or nil, layout = name, ratio = number }
local spaces = {}
-- Window id -> workspace index.
local where = {}
-- Window id -> true for windows that are fullscreen.
local fullscreen = {}
-- Window id -> { x, y, w, h, layer } as last arranged (layer: "tile",
-- "float" or "full").
local rects = {}
-- What each floating window was last arranged for (see `P.window_changed`).
local seen = {}
-- Window id -> the layer its client was last told about (see `tell`).
local told = {}
-- Window id -> the window it belongs to (a dialog's parent), as last arranged.
local parents = {}

-- Whether a window belongs to one that is fullscreen (a dialog of it, or a
-- dialog of a dialog): it has to be above that window, not under it with
-- the other floating windows.
local function over_fullscreen(id)
    local parent = parents[id]
    for _ = 1, 8 do
        if not parent then
            return false
        elseif fullscreen[parent] then
            return true
        end
        parent = parents[parent]
    end
    return false
end
local active = 1

-- Several windows can go at once (a client that exits with two windows, a
-- session that ends), and this script hears of them one at a time: a window
-- it still lists may be gone already, and the core refuses a call that
-- names one. Such a call is simply dropped; `P.window_closed` forgets the
-- windows that are gone.
local function try(call, ...)
    return (pcall(call, ...))
end

local function ensure()
    for i = 1, P.settings.workspaces do
        if not spaces[i] then
            spaces[i] = { order = {}, focus = nil, layout = "tall", ratio = P.settings.ratio }
        end
    end
end

local function index_of(list, id)
    for i, other in list do
        if other == id then
            return i
        end
    end
    return nil
end

local function window_table()
    local windows = {}
    for _, window in hz.windows() do
        windows[window.id] = window
    end
    return windows
end

-- Whether a window stays out of the tiles.
local function floats(window)
    if window.parent ~= nil then
        return true
    end
    return window.min_width > 0
        and window.min_width == window.max_width
        and window.min_height > 0
        and window.min_height == window.max_height
end

-- The rectangles of `count` tiles in the area (x, y, w, h).
local function tiles(layout, ratio, count, x, y, w, h)
    local gap = P.settings.gap
    local list = {}
    if count == 0 then
        return list
    end
    if layout == "columns" or count == 1 then
        local each = (w - gap * (count - 1)) // count
        for i = 1, count do
            local last = i == count
            local left = x + (i - 1) * (each + gap)
            table.insert(list, { x = left, y = y, w = if last then x + w - left else each, h = h })
        end
        return list
    end
    local main = math.floor((w - gap) * ratio + 0.5)
    table.insert(list, { x = x, y = y, w = main, h = h })
    local rest = count - 1
    local side_x, side_w = x + main + gap, w - main - gap
    local each = (h - gap * (rest - 1)) // rest
    for i = 1, rest do
        local last = i == rest
        local top = y + (i - 1) * (each + gap)
        table.insert(list, { x = side_x, y = top, w = side_w, h = if last then y + h - top else each })
    end
    return list
end

-- Tells a window's client what it is: a tile, floating, or fullscreen. A
-- tile is told that it is maximized: it has no say over its size, and
-- should draw no shadow or rounded corners of its own and fill the size it
-- is given exactly (a terminal would otherwise round it to whole cells).
local function tell(id, layer)
    if told[id] ~= layer then
        told[id] = layer
        try(hz.set_states, id, { maximized = layer == "tile", fullscreen = layer == "full" })
    end
end

-- Recomputes the place and size of every window of every workspace, and the
-- stacking order.
function P.arrange()
    ensure()
    local output = hz.outputs()[1]
    if not output then
        return
    end
    local windows = window_table()
    local gap = P.settings.gap
    local ax, ay = output.x + gap, output.y + gap
    local aw = output.logical_width - gap * 2
    local ah = output.logical_height - gap * 2 - P.settings.bottom
    local stack = {}
    local function arrange_space(space)
        local tiled, floating, full = {}, {}, {}
        for _, id in space.order do
            local window = windows[id]
            if window then
                if fullscreen[id] then
                    table.insert(full, id)
                elseif floats(window) then
                    table.insert(floating, id)
                else
                    table.insert(tiled, id)
                end
            end
        end
        for i, rect in tiles(space.layout, space.ratio, #tiled, ax, ay, aw, ah) do
            local id = tiled[i]
            rect.layer = "tile"
            rects[id] = rect
            tell(id, "tile")
            try(hz.place, id, rect.x, rect.y)
            try(hz.configure, id, rect.w, rect.h)
        end
        for _, id in floating do
            local window = windows[id]
            local over = window.parent and rects[window.parent]
            local x, y, w, h = output.x, output.y, output.logical_width, output.logical_height
            if over then
                x, y, w, h = over.x, over.y, over.w, over.h
            end
            parents[id] = window.parent
            local fw, fh = window.width or 0, window.height or 0
            local rect = { x = x + (w - fw) // 2, y = y + (h - fh) // 2, w = fw, h = fh, layer = "float" }
            rects[id] = rect
            tell(id, "float")
            try(hz.configure, id, nil, nil)
            try(hz.place, id, rect.x, rect.y)
        end
        for _, id in full do
            rects[id] = { x = output.x, y = output.y, w = output.logical_width, h = output.logical_height, layer = "full" }
            tell(id, "full")
            try(hz.place, id, output.x, output.y)
            try(hz.configure, id, output.logical_width, output.logical_height)
        end
        local above = {}
        for _, id in tiled do
            table.insert(stack, id)
        end
        for _, id in floating do
            table.insert(if over_fullscreen(id) then above else stack, id)
        end
        for _, list in { full, above } do
            for _, id in list do
                table.insert(stack, id)
            end
        end
    end
    -- The workspace in view goes on top.
    for i, space in spaces do
        if i ~= active then
            arrange_space(space)
        end
    end
    arrange_space(spaces[active])
    if #stack > 0 then
        try(hz.restack, stack)
    end
    hz.request_frame()
end

local function focused()
    for _, window in hz.windows() do
        if window.focused then
            return window.id
        end
    end
    return nil
end

-- Gives the focus to a window and remembers it as its workspace's.
local function focus(id)
    if id and where[id] then
        spaces[where[id]].focus = id
    end
    if not try(hz.focus, id) then
        hz.focus(nil)
    end
end

function P.init(saved)
    spaces, where, fullscreen, rects, seen, told, parents = {}, {}, {}, {}, {}, {}, {}
    active = 1
    ensure()
    local present = window_table()
    if saved and saved.spaces then
        for i, old in saved.spaces do
            local space = spaces[i]
            if space then
                space.layout = old.layout or space.layout
                space.ratio = old.ratio or space.ratio
                for _, id in old.order do
                    if present[id] and not where[id] then
                        table.insert(space.order, id)
                        where[id] = i
                    end
                end
                if old.focus and where[old.focus] == i then
                    space.focus = old.focus
                end
            end
        end
        if saved.active and spaces[saved.active] then
            active = saved.active
        end
        for _, id in saved.fullscreen or {} do
            if present[id] then
                fullscreen[id] = true
            end
        end
    end
    -- Windows nobody handed over go to the workspace in view, left to
    -- right as they are on screen.
    local others = {}
    for _, window in hz.windows() do
        if not where[window.id] then
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
        table.insert(spaces[active].order, window.id)
        where[window.id] = active
    end
    P.arrange()
end

function P.save()
    local full = {}
    for id in fullscreen do
        table.insert(full, id)
    end
    local list = {}
    for i, space in spaces do
        list[i] = { order = space.order, focus = space.focus, layout = space.layout, ratio = space.ratio }
    end
    return { spaces = list, active = active, fullscreen = full }
end

-- What the others read ---------------------------------------------------

function P.active()
    return active
end

function P.count()
    return P.settings.workspaces
end

function P.workspace_of(id)
    return where[id]
end

-- { x, y, w, h, layer } of a window, on its own workspace.
function P.rect_of(id)
    return rects[id]
end

-- The window ids of a workspace, bottom to top.
function P.windows_of(index)
    local space = spaces[index]
    local tiled, floating, full, above = {}, {}, {}, {}
    for _, id in space.order do
        local rect = rects[id]
        if rect then
            if rect.layer == "full" then
                table.insert(full, id)
            elseif rect.layer ~= "float" then
                table.insert(tiled, id)
            elseif over_fullscreen(id) then
                table.insert(above, id)
            else
                table.insert(floating, id)
            end
        end
    end
    for _, list in { floating, full, above } do
        for _, id in list do
            table.insert(tiled, id)
        end
    end
    return tiled
end

function P.order_of(index)
    return spaces[index].order
end

function P.focus_of(index)
    return spaces[index].focus
end

function P.is_fullscreen(id)
    return fullscreen[id] == true
end

-- Events -----------------------------------------------------------------

function P.window_opened(id)
    if where[id] then
        return
    end
    local windows = window_table()
    local window = windows[id]
    -- A dialog opens where its parent is; anything else in view.
    local index = active
    if window and window.parent and where[window.parent] then
        index = where[window.parent]
    end
    local space = spaces[index]
    -- A window that is fullscreen there would hide the new one, which gets
    -- the focus: it goes back to its tile. Its own dialogs open above it.
    for _, other in space.order do
        if fullscreen[other] and not (window and window.parent == other) then
            fullscreen[other] = nil
        end
    end
    -- After the window that has the focus, so that the main window stays.
    local at = space.focus and index_of(space.order, space.focus)
    table.insert(space.order, if at then at + 1 else #space.order + 1, id)
    where[id] = index
    seen[id] = "tiled"
    if window and floats(window) then
        seen[id] = nil
    end
    P.arrange()
    if index == active then
        focus(id)
    else
        space.focus = id
    end
end

-- Takes a window out of its workspace.
local function forget(id)
    local index = where[id]
    if not index then
        return
    end
    local space = spaces[index]
    local at = index_of(space.order, id)
    table.remove(space.order, at)
    where[id] = nil
    fullscreen[id] = nil
    rects[id] = nil
    seen[id] = nil
    told[id] = nil
    parents[id] = nil
    if space.focus == id then
        -- The neighbour that moves into the gap.
        space.focus = space.order[math.min(at, #space.order)]
    end
end

function P.window_closed(id)
    local index = where[id]
    if not index then
        return
    end
    forget(id)
    -- Others may have gone with it (see `try`).
    local present = window_table()
    local gone = {}
    for other in where do
        if not present[other] then
            table.insert(gone, other)
        end
    end
    for _, other in gone do
        forget(other)
    end
    P.arrange()
    if index == active then
        focus(spaces[index].focus)
    end
end

-- A floating window is centered again when its size changes; a window
-- whose parent or size limits changed may move in or out of the tiles.
function P.window_changed(id)
    if not where[id] then
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
                P.arrange()
            end
            return
        end
    end
end

-- The focus moved for a reason this script did not give (a click).
function P.focus_changed(id)
    if id and where[id] then
        spaces[where[id]].focus = id
        if where[id] ~= active then
            P.switch(where[id])
        end
    end
end

function P.set_fullscreen(id, on)
    if not where[id] then
        return
    end
    fullscreen[id] = on or nil
    P.arrange()
    if on and where[id] == active then
        focus(id)
    end
end

function P.window_request(id, request)
    if request.kind == "fullscreen" then
        P.set_fullscreen(id, true)
    elseif request.kind == "unfullscreen" then
        P.set_fullscreen(id, false)
    elseif request.kind == "activate" then
        if request.from_input and where[id] then
            P.switch(where[id])
            focus(id)
        end
    end
end

-- Actions ----------------------------------------------------------------

-- Brings a workspace into view. Returns whether the view changed.
function P.switch(index)
    index = math.clamp(index, 1, P.settings.workspaces)
    if index == active then
        return false
    end
    active = index
    P.arrange()
    local space = spaces[active]
    if space.focus and not where[space.focus] then
        space.focus = nil
    end
    focus(space.focus or space.order[1])
    return true
end

-- Sends a window to another workspace. The view stays unless `follow`.
function P.send(id, index, follow)
    index = math.clamp(index, 1, P.settings.workspaces)
    local from = where[id]
    if not from or from == index then
        return false
    end
    local source, target = spaces[from], spaces[index]
    local at = index_of(source.order, id)
    table.remove(source.order, at)
    if source.focus == id then
        source.focus = source.order[math.min(at, #source.order)]
    end
    table.insert(target.order, id)
    target.focus = id
    where[id] = index
    if follow then
        active = index
    end
    P.arrange()
    focus(spaces[active].focus)
    return true
end

-- The window that keys act on: the focused one, if it is in view.
function P.current()
    local id = focused()
    if id and where[id] == active then
        return id
    end
    return nil
end

-- Moves the focus (or, with `swap`, the focused window) to the next (1) or
-- previous (-1) place in the order of the workspace in view.
function P.step(direction, swap)
    local space = spaces[active]
    local current = P.current()
    local at = current and index_of(space.order, current)
    if not at then
        if not swap and #space.order > 0 then
            focus(space.order[1])
        end
        return
    end
    local target = at + direction
    if target < 1 or target > #space.order then
        return
    end
    if swap then
        space.order[at], space.order[target] = space.order[target], space.order[at]
        P.arrange()
    else
        focus(space.order[target])
    end
end

-- Puts `id` where `other` is in the order, and `other` where `id` was.
function P.swap(id, other)
    local index = where[id]
    if not index or where[other] ~= index or id == other then
        return false
    end
    local order = spaces[index].order
    local a, b = index_of(order, id), index_of(order, other)
    order[a], order[b] = order[b], order[a]
    P.arrange()
    return true
end

function P.focus(id)
    if where[id] then
        if where[id] ~= active then
            P.switch(where[id])
        end
        focus(id)
    end
end

-- Makes the focused window the main one (or, if it is, the next one).
function P.promote()
    local space = spaces[active]
    local current = P.current()
    local at = current and index_of(space.order, current)
    if not at or #space.order < 2 then
        return
    end
    if at == 1 then
        at = 2
    end
    local id = table.remove(space.order, at)
    table.insert(space.order, 1, id)
    P.arrange()
    focus(id)
end

function P.cycle_layout()
    local space = spaces[active]
    space.layout = if space.layout == "tall" then "columns" else "tall"
    P.arrange()
end

function P.grow(amount)
    local space = spaces[active]
    space.ratio = math.clamp(space.ratio + amount, 0.25, 0.75)
    P.arrange()
end

-- For tools and tests.
function P.describe()
    local list = {}
    for i, space in spaces do
        list[i] = { order = space.order, focus = space.focus, layout = space.layout }
    end
    return { active = active, spaces = list }
end

return P
