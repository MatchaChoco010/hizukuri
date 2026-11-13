-- Tiling with animated windows.
--
-- The built-in tiling (`tile`) decides where every window goes, as without
-- animation: logo+H / logo+L move the focus, logo+shift+H / logo+shift+L
-- swap the focused window with its neighbour, logo+Q closes it. This
-- project changes only how windows get there: instead of jumping, a window
-- moves from where it is drawn to its new place, a new window fades and
-- grows in, and a closed one fades and shrinks out.
--
-- Nothing in the core animates. Every frame, `animate` computes from the
-- frame's time where each window is drawn and how opaque, and the built-in
-- renderer (`render`) draws that. See `animate.luau` for how.
--
-- `render`, `tile` and `affine` are not in this directory: a project uses
-- the modules of the built-in project it does not have itself, and so the
-- shaders `render` names come from the built-in project as well.

local render = hz.include("render")
local tile = hz.include("tile")
local animate = hz.include("animate")

-- Settings: change them, then reload (Ctrl+Alt+Shift+R).
animate.settings = {
    -- Seconds a window takes to move to a new place or size.
    move_duration = 0.30,
    -- How the motion speeds up and slows down: a name from
    -- `animate.easings` (linear, ease_out_cubic, ease_in_out_cubic,
    -- ease_out_expo, ease_out_back).
    move_easing = "ease_out_cubic",
    -- A window that opens: from this scale and transparent, to full size.
    open_duration = 0.25,
    open_easing = "ease_out_cubic",
    open_scale = 0.8,
    -- A window that closes: to this scale and transparent.
    close_duration = 0.20,
    close_easing = "ease_in_out_cubic",
    close_scale = 0.8,
}

function init(saved)
    render.init()
    tile.init(saved and saved.tile)
    -- Windows that are already there (after a reload) stand where they are.
    animate.init()
end

function unload()
    return { tile = tile.save() }
end

-- The core calls this when it draws a frame of an output. `time` is when the
-- frame is expected on screen: positions are computed for that moment.
function frame(output, target, time)
    render.frame(output, target, animate.frame(time))
end

-- Input goes where the windows are drawn, also half way through a move.
function hit_test(x, y, accepts)
    return render.hit_test(x, y, accepts, animate.transform_of)
end

function locate(surface, x, y)
    return render.locate(surface, x, y, animate.transform_of)
end

-- Window management is the built-in tiling's. After each event the
-- animations look at where the windows are meant to be now.

function window_opened(id)
    tile.window_opened(id)
    animate.window_opened(id)
end

function window_changed(id)
    tile.window_changed(id)
    animate.retarget()
end

function window_closed(id)
    tile.window_closed(id)
    animate.retarget()
end

-- A client asks for something (to be fullscreen, to be activated, ...).
function window_request(id, request)
    tile.window_request(id, request)
    animate.retarget()
end

-- Touchpad gestures: a three finger swipe moves the focus between the
-- tiles. Nothing moves, so there is nothing to animate.
gesture_begin = tile.gesture_begin
gesture_update = tile.gesture_update
gesture_end = tile.gesture_end

function outputs_changed()
    tile.outputs_changed()
    animate.retarget()
end

function key(keycode, pressed, mods)
    local used = tile.key(keycode, pressed, mods)
    animate.retarget()
    return used
end

-- Hooks for tools and tests, reached through the control interface
-- (`script_call`).

function tile_order()
    return tile.order()
end

-- Where each window is drawn: { id, x, y, width, height, opacity, closing }.
function animation_state()
    return animate.describe()
end

-- Runs the work of one frame `count` times without drawing, for measuring
-- what the animations cost.
function animation_bench(count)
    animate.bench(count)
end
