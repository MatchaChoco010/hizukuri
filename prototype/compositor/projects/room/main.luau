-- A room with monitors in it.
--
-- The picture is a dark room seen from a chair: a desk, and monitors on
-- it. Windows are shown on those monitors, one each. A monitor without a
-- window says so (NO SIGNAL). The monitors are the lights of the room:
-- what they show lights the desk and the walls.
--
-- Keys (with the logo key held):
--   H / J / K / L, arrows   focus the monitor to the left / below / above / right
--   1 .. 4                  focus a monitor by its number
--   Shift + direction       trade windows with the monitor in that direction
--   Tab / Shift+Tab         show the next / previous waiting window on the
--                           focused monitor
--   F                       move up close to the focused monitor, so that it
--                           fills the view; again to sit back
--   Return                  start a terminal
--   D                       open the launcher on the focused monitor (`menu`):
--                           type to search, Up / Down to choose, Enter to
--                           start, Escape (or logo+D again) to close
--   V                       switch the way out between the plain conversion
--                           and a view that maps tones (OpenColorIO)
--   Q                       ask the focused window to close
--   Escape                  free a held pointer; else sit back
--
-- The pointer works on the monitors as they are seen: the point under it
-- is found by following the view ray from the camera to the monitor's
-- surface.
--
-- Everything here is this project's: the scene (`scene`), the camera
-- (`camera`), which window is where (`wm`), and the drawing (`draw` and
-- `shaders/`). The core only hands over the windows' contents and the
-- input.

local draw = hz.include("draw")
local scene = hz.include("scene")
local camera = hz.include("camera")
local wm = hz.include("wm")
local menu = hz.include("menu")

-- Settings: change them, then reload (Ctrl+Alt+Shift+R).

-- Quality against speed. Lower these first when frames take too long.
-- Samples per pixel of the picture of the room (edges): 4, 2, or 1 (off).
draw.settings.samples = 4
-- Light that spreads around the bright parts, in a view of the room. 0
-- turns it off (two passes less).
draw.settings.bloom = 0.5
-- Whether the things on the desk shadow it (per thing and monitor, about
-- the cost of one more light on the pixels of the desk).
draw.settings.shadows = true
-- How a monitor is blurred when it lights the room: "gaussian" (smooth; a
-- second image per monitor, `light_scale` of its resolution, blurred level
-- by level whenever its picture changes) or "mips" (plain mip levels: a
-- software renderer should use this, with `shadows = false` and
-- `samples = 1`).
draw.settings.light_filter = "gaussian"
draw.settings.light_scale = 0.5

-- The look. How strongly the screens light the room (diffuse light, and
-- glossy reflections), how much the desk throws back, the light from
-- nowhere, and how far the room brightens when the screens are dark (0:
-- not at all; then a dark screen leaves the room dark).
draw.settings.light = 14.0
draw.settings.gloss = 4.0
draw.settings.bounce = 10.0
draw.settings.ambient = 0.002
draw.settings.adapt = 0.3
-- Darker corners, and how far (meters) the view moves with the pointer.
draw.settings.vignette = 0.35
draw.settings.parallax = 0.02
-- Seconds the camera takes to move, and a monitor to change its picture.
camera.settings.duration = 0.55
wm.settings.switch_duration = 0.4
-- The program logo+Return starts. nil: the environment variable
-- HIZUKURI_TERMINAL, else `foot` (the launcher starts applications that
-- run in a terminal in that one, with `-e`).
wm.settings.terminal = nil
-- How far the camera moves towards the monitor of the launcher (0..1).
menu.settings.closer = 0.45

function init(saved)
    draw.init()
    camera.reset()
    draw.goal_key = nil
    wm.changed = function(t)
        -- The camera sets off when the event happens, not when the next
        -- frame is drawn.
        draw.aim(nil, t)
        hz.request_frame()
    end
    menu.changed = wm.changed
    menu.now = function()
        return wm.tick(hz.time())
    end
    menu.focused = wm.focused
    menu.monitors = scene.monitors
    menu.restore(saved and saved.launcher)
    wm.init(scene.monitors, saved and saved.wm)
    hz.request_frame()
end

function unload()
    return { wm = wm.save(), launcher = menu.save() }
end

-- What the core was last told about each window: whether it is seen.
local told = {}

-- Tells the core which windows are seen (`hz.set_visible`): the others
-- (waiting for a monitor, or on another monitor while the camera is up
-- close on one) get no frame callbacks and are told they are suspended,
-- so that their clients stop drawing.
local function tell_visibility()
    local seen = wm.seen(draw.only_focused)
    local now_told = {}
    for _, window in hz.windows() do
        local visible = seen[window.id] == true or not draw.settings.hide_unseen
        if told[window.id] ~= visible and not pcall(hz.set_visible, window.id, visible) then
            visible = nil
        end
        now_told[window.id] = visible
    end
    told = now_told
end

function frame(output, target, time)
    local t = wm.tick(time)
    local switching = wm.step(t)
    local repeating = menu.tick(t)
    local animated = draw.frame(output, target, t)
    tell_visibility()
    if animated or switching or repeating then
        hz.request_frame()
    end
end

-- The topmost surface under a point of the screen: the view ray from the
-- camera picks the monitor and the point on it; the windows that monitor
-- shows are asked from the top.
function hit_test(x, y, accepts)
    local monitor, px, py = draw.pick(x, y)
    if not monitor or (menu.is_open() and monitor.index == menu.monitor_at(hz.time())) then
        -- (The launcher covers what its monitor shows.)
        return nil
    end
    local slot = wm.slot(monitor.index)
    if not slot.window or slot.transition then
        return nil
    end
    local items = wm.items(monitor, slot.window)
    for i = #items, 1, -1 do
        local item = items[i]
        local t = item.transform
        local wx, wy = (px - t[5]) / t[1], (py - t[6]) / t[4]
        local surfaces = hz.surfaces(item.id)
        for j = #surfaces, 1, -1 do
            local surface = surfaces[j]
            local sx, sy = wx - surface.x, wy - surface.y
            if accepts(surface.id, sx, sy) then
                return surface.id, sx, sy
            end
        end
    end
    return nil
end

-- A point of the screen in the coordinates of one given surface, wherever
-- the point is: on the plane of the monitor that shows it.
function locate(surface_id, x, y)
    for _, window in hz.windows() do
        for _, surface in hz.surfaces(window.id) do
            if surface.id == surface_id then
                local index = wm.monitor_of(window.id)
                local monitor = index and scene.monitors[index]
                if not monitor or not window.width then
                    return nil
                end
                local px, py = draw.pick_on(monitor, x, y)
                if not px then
                    return nil
                end
                local t = wm.fit(monitor, window.width, window.height)
                return (px - t[5]) / t[1] - surface.x, (py - t[6]) / t[4] - surface.y
            end
        end
    end
    return nil
end

-- Where a point of a surface is seen on the screen: on the monitor that
-- shows its window, through the camera. The core asks it to tell an input
-- method outside (the parent's, when nested) where the text cursor is.
function project(surface_id, x, y)
    for _, window in hz.windows() do
        for _, surface in hz.surfaces(window.id) do
            if surface.id == surface_id then
                local index = wm.monitor_of(window.id)
                local monitor = index and scene.monitors[index]
                if not monitor or not window.width then
                    return nil
                end
                local t = wm.fit(monitor, window.width, window.height)
                return draw.project_on(monitor, (x + surface.x) * t[1] + t[5], (y + surface.y) * t[4] + t[6])
            end
        end
    end
    return nil
end

window_opened = wm.window_opened
window_closed = wm.window_closed
window_changed = wm.window_changed
window_request = wm.window_request
-- A click: the launcher's while it is open, else the window manager's.
function pointer_button(button, pressed, window, surface, mods)
    if menu.is_open() then
        if pressed then
            local x, y = hz.pointer()
            local monitor, px, py = nil, nil, nil
            if x then
                monitor, px, py = draw.pick(x, y)
            end
            menu.click(monitor, px, py)
        end
        return true
    end
    return wm.pointer_button(button, pressed, window, surface, mods)
end

-- Whether the picture goes out through a view that maps tones (logo+V),
-- and the exposure (in stops) that view is given.
local tone_mapped = false
local view_exposure = 1.5

-- Switches the way out of every output between the plain conversion and
-- the first tone mapping view of the default display of the OpenColorIO
-- configuration (an "ACES ..." view).
local function toggle_view()
    local info = hz.ocio_info and hz.ocio_info()
    if not info then
        hz.log("room: no OpenColorIO configuration; the view stays as it is")
        return
    end
    tone_mapped = not tone_mapped
    -- The view maps the tones: the renderer leaves the bright parts of its
    -- surfaces as they are.
    draw.settings.shoulder = not tone_mapped
    -- Such a view puts the white of a screen at a grey; some exposure
    -- brings it back up.
    for _, output in hz.outputs() do
        hz.ocio_set_controls(output.id, { exposure = if tone_mapped then view_exposure else 0, gamma = 1 })
    end
    if not tone_mapped then
        hz.ocio_set_output(nil, nil, nil)
    else
        for _, display in info.displays do
            if display.name == info.default_display then
                local chosen = display.default_view
                for _, view in display.views do
                    if string.find(view, "^ACES") then
                        chosen = view
                        break
                    end
                end
                hz.ocio_set_output(nil, display.name, chosen)
                hz.log("room: view " .. tostring(chosen))
            end
        end
    end
    hz.request_frame()
end

local KEY_V = 47

function key(keycode, pressed, mods, k)
    if menu.key(keycode, pressed, mods, k) then
        return true
    end
    if mods.logo and keycode == KEY_V then
        if pressed then
            toggle_view()
        end
        return true
    end
    return wm.key(keycode, pressed, mods)
end

function outputs_changed()
    hz.request_frame()
end

-- The key that is the logo key changed (nested): the launcher says which.
function logo_key_changed(_key)
    hz.request_frame()
end

-- Hooks for tools and tests, reached through the control interface
-- (`script_call`).

-- Changes a setting of the renderer (`draw.settings`) while running, for
-- trying settings out and for measuring. The settings that say what is
-- made at the start (`format`, `light_filter`, `light_scale`) need a
-- reload instead.
function room_set(name, value)
    if draw.settings[name] == nil then
        error("no such setting: " .. tostring(name))
    end
    draw.settings[name] = value
    hz.request_frame()
end

-- Sets the command line logo+Return starts (a list of arguments).
function room_terminal(argv)
    wm.settings.terminal = argv
end

-- What every monitor shows, which windows wait, which monitor has the
-- focus.
function room_state()
    local state = wm.describe()
    state.launcher = menu.describe()
    local last = draw.last
    if last then
        state.camera = { eye = last.pose.eye, rotation = last.pose.rotation, focal = last.pose.focal }
    end
    return state
end

-- The middle of a line of the launcher: { monitor, x, y } in the pixels of
-- its monitor (for `monitor_point`).
function room_launcher_line(line)
    return menu.line_point(line)
end

-- Where a point of a monitor (in its pixels) is on the screen (global
-- logical coordinates), as the last frame was drawn. Nil when it is behind
-- the camera.
function monitor_point(index, px, py)
    local monitor = scene.monitors[index]
    local last = draw.last
    if not monitor or not last then
        return nil
    end
    local m3 = hz.include("math3d")
    local u, v = px / monitor.pixels_x - 0.5, 0.5 - py / monitor.pixels_y
    local world = m3.madd(m3.madd(monitor.center, monitor.right, u * monitor.width), monitor.up, v * monitor.height)
    local x, y, _, w = m3.mat4_transform(camera.view_projection(last.pose, last.aspect), world)
    if w <= 0 then
        return nil
    end
    return { last.x + (x / w * 0.5 + 0.5) * last.width, last.y + (y / w * 0.5 + 0.5) * last.height }
end
