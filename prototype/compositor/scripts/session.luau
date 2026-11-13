-- The built-in session: a color managed renderer, and a tiling window
-- manager when the session leaves window management to the script.
--
-- The functions below are what the core calls; they are globals. Everything
-- they do goes through `gfx` (the GPU) and `hz` (the compositor core).

local render = hz.include("render")
local tile = hz.manages_windows() and hz.include("tile") or nil

function init(saved)
    render.init()
    if saved then
        render.restore(saved.transforms)
    end
    if tile then
        tile.init(saved and saved.tile)
    end
end

-- What to carry over to the script that replaces this one.
function unload()
    return {
        transforms = render.save(),
        tile = tile and tile.save() or nil,
    }
end

function frame(output, target)
    render.frame(output, target)
end

function hit_test(x, y, accepts)
    return render.hit_test(x, y, accepts)
end

function locate(surface, x, y)
    return render.locate(surface, x, y)
end

if tile then
    window_opened = tile.window_opened
    window_changed = tile.window_changed
    window_closed = tile.window_closed
    window_request = tile.window_request
    outputs_changed = tile.outputs_changed
    key = tile.key
    gesture_begin = tile.gesture_begin
    gesture_update = tile.gesture_update
    gesture_end = tile.gesture_end
end

-- Hooks for tools and tests, reached through the control interface
-- (`script_call`).

-- Draws a window with the transform { a, b, c, d, tx, ty } (window-local to
-- global logical coordinates). Without a transform, back to its position.
function set_transform(window, transform)
    render.set_transform(window, transform)
end

-- Draws a window brighter or darker: a factor on its light (1 or nothing:
-- its reference white lands on the reference white of the output).
function set_brightness(window, factor)
    render.set_brightness(window, factor)
end

-- Chooses the primaries of the linear working space.
function set_working_space(name)
    hz.set_working_space(name)
end

function working_space()
    return (hz.working_space())
end

function tile_order()
    return tile and tile.order() or nil
end
