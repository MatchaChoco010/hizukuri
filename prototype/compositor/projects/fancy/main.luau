-- fancy: a tiling compositor with workspaces, written as a project.
--
-- The idea, in three parts:
--
--   One strip, one camera. All workspaces lie side by side on one long
--     strip. The screen is a camera on that strip. Switching workspace is
--     the camera travelling; while it travels it pulls back, and the
--     workspaces pass by as plates in front of a wall. The overview is the
--     same camera, pulled far back. A window sent to another workspace
--     travels along the strip to get there. Nothing is ever swapped for
--     something else: everything has a place, and the eye can follow it.
--
--   Springs. Everything that moves is on a spring that keeps its speed
--     when its target changes. A motion that is interrupted turns round; it
--     never jumps and never starts from standstill again.
--
--   Heat. The window with the focus is hot: its rim glows like metal out
--     of the fire and flares when the focus arrives; a window that loses
--     the focus cools to grey. Behind everything, embers glow; they are
--     stirred by what happens and settle again. When nothing happens,
--     nothing moves and nothing is drawn.
--
-- Where things are: `space` (workspaces, tiling, focus), `stage` (the
-- camera, the springs, what is drawn where), `paint` (the renderer),
-- `spring` (the springs), `menu` (the launcher), `shaders/` (window,
-- frame, backdrop). `affine`, `launcher` (what the launcher does) and the
-- encoding shader are the built-in project's.
--
-- Keys (with the logo key held):
--
--   H / K, L / J         focus the previous / next window
--   Shift + H/K, L/J     swap the focused window with the previous / next
--   M                    make the focused window the main one
--   Space                the other layout (main and stack / columns)
--   - / =                the main window narrower / wider
--   F                    fullscreen on and off
--   Q                    ask the focused window to close
--   Return               a terminal
--   1 .. 5               go to a workspace
--   Shift + 1 .. 5       send the focused window to a workspace
--   Ctrl + H, L          the workspace to the left / right (also , and .)
--   Ctrl + Shift + H, L  take the focused window there
--   Tab                  the overview, and back
--   Escape               leave the overview; free the pointer from a lock
--   D                    the launcher: type to narrow the applications
--                        down, Up / Down to choose, Return to start one,
--                        Escape (or D again) to close (`menu.luau`)
--
-- In the overview: H / L (or the arrows) choose, Return or a click goes
-- there.
--
-- Pointer: with the logo key held, the left button picks a window up; drop
-- it on another to swap them. Touchpad: three fingers sideways move along
-- the strip.

local affine = hz.include("affine")
local space = hz.include("space")
local stage = hz.include("stage")
local paint = hz.include("paint")
local menu = hz.include("menu")

local settings = {
    -- The program started with Return, when the environment does not name
    -- one (HIZUKURI_TERMINAL).
    terminal = "foot",
}

local KEY = {
    ESC = 1,
    N1 = 2,
    MINUS = 12,
    EQUAL = 13,
    TAB = 15,
    Q = 16,
    ENTER = 28,
    D = 32,
    F = 33,
    H = 35,
    J = 36,
    K = 37,
    L = 38,
    M = 50,
    COMMA = 51,
    DOT = 52,
    SPACE = 57,
    LEFT = 105,
    RIGHT = 106,
}
local BUTTON_LEFT = 272

function init(saved)
    paint.init()
    space.init(saved and saved.space)
    stage.init()
    menu.init(saved and saved.menu)
end

function unload()
    return { space = space.save(), menu = menu.save() }
end

function frame(output, target, time)
    local scene = stage.frame(time, output)
    menu.frame(time, output, scene)
    paint.frame(output, target, scene)
end

-- Input goes where the windows are drawn, also while they move. In the
-- overview nothing reaches the windows: a click there chooses.
function hit_test(x, y, accepts)
    if stage.in_overview() or menu.covers(x, y) then
        return nil
    end
    -- Topmost first, in the order the last frame drew them.
    local order = stage.drawn_order()
    for i = #order, 1, -1 do
        local id = order[i]
        local transform, width, height = stage.contents_of(id)
        local inverse = transform and affine.invert(transform)
        if inverse then
            local wx, wy = affine.apply(inverse, x, y)
            local within = wx >= 0 and wy >= 0 and wx < width and wy < height
            local surfaces = hz.surfaces(id)
            for j = #surfaces, 1, -1 do
                local surface = surfaces[j]
                -- A window is cut to its rectangle; its menus are not.
                local cut = surface.role == "toplevel" or surface.role == "subsurface"
                if within or not cut then
                    local sx, sy = wx - surface.x, wy - surface.y
                    if accepts(surface.id, sx, sy) then
                        return surface.id, sx, sy
                    end
                end
            end
        end
    end
    return nil
end

function locate(surface_id, x, y)
    for _, window in hz.windows() do
        for _, surface in hz.surfaces(window.id) do
            if surface.id == surface_id then
                local transform = stage.contents_of(window.id) or affine.translate(window.x, window.y)
                local inverse = affine.invert(transform)
                if not inverse then
                    return nil
                end
                local wx, wy = affine.apply(inverse, x, y)
                return wx - surface.x, wy - surface.y
            end
        end
    end
    return nil
end

-- Window management is `space`'s. After each event the stage looks at where
-- things belong now.

function window_opened(id)
    space.window_opened(id)
    stage.window_opened(id)
end

function window_changed(id)
    space.window_changed(id)
    stage.retarget()
end

function window_closed(id)
    space.window_closed(id)
    stage.retarget()
end

function focus_changed(id)
    space.focus_changed(id)
    stage.retarget()
end

function window_request(id, request)
    space.window_request(id, request)
    stage.retarget()
end

function outputs_changed()
    space.arrange()
    stage.retarget()
end

local function start_terminal()
    local argv = {}
    for word in string.gmatch(hz.env("HIZUKURI_TERMINAL") or settings.terminal, "%S+") do
        table.insert(argv, word)
    end
    local pid, problem = hz.spawn(argv)
    if not pid then
        hz.log("fancy: the terminal did not start: " .. tostring(problem))
    end
end

-- What a key does: returns true when the key is this script's.
local function bound(keycode, mods)
    local shift, ctrl = mods.shift, mods.ctrl
    if stage.in_overview() then
        if keycode == KEY.H or keycode == KEY.LEFT then
            space.switch(space.active() - 1)
        elseif keycode == KEY.L or keycode == KEY.RIGHT then
            space.switch(space.active() + 1)
        elseif keycode == KEY.ENTER or keycode == KEY.ESC or keycode == KEY.TAB then
            stage.set_overview(false)
        elseif keycode >= KEY.N1 and keycode < KEY.N1 + space.count() then
            space.switch(keycode - KEY.N1 + 1)
        else
            return false
        end
        return true
    end
    if keycode >= KEY.N1 and keycode < KEY.N1 + space.count() then
        local index = keycode - KEY.N1 + 1
        if shift then
            local current = space.current()
            if current then
                space.send(current, index, false)
            end
        else
            space.switch(index)
        end
    elseif ctrl and (keycode == KEY.H or keycode == KEY.L) then
        local index = space.active() + (if keycode == KEY.L then 1 else -1)
        local current = space.current()
        if shift and current then
            space.send(current, index, true)
        else
            space.switch(index)
        end
    elseif keycode == KEY.COMMA then
        space.switch(space.active() - 1)
    elseif keycode == KEY.DOT then
        space.switch(space.active() + 1)
    elseif keycode == KEY.H or keycode == KEY.K then
        space.step(-1, shift)
    elseif keycode == KEY.L or keycode == KEY.J then
        space.step(1, shift)
    elseif keycode == KEY.M then
        space.promote()
    elseif keycode == KEY.SPACE then
        space.cycle_layout()
    elseif keycode == KEY.MINUS then
        space.grow(-0.04)
    elseif keycode == KEY.EQUAL then
        space.grow(0.04)
    elseif keycode == KEY.F then
        local current = space.current()
        if current then
            space.set_fullscreen(current, not space.is_fullscreen(current))
        end
    elseif keycode == KEY.Q then
        local current = space.current()
        if current then
            hz.close(current)
        end
    elseif keycode == KEY.ENTER then
        start_terminal()
    elseif keycode == KEY.TAB then
        stage.set_overview(true)
    elseif keycode == KEY.D then
        menu.open()
        stage.stir(0.5)
    elseif keycode == KEY.ESC then
        hz.release_pointer_constraint()
    else
        return false
    end
    return true
end

function key(keycode, pressed, mods, k)
    -- While the launcher is open, every key is its own.
    if menu.is_open() then
        if pressed and mods.logo and keycode == KEY.D then
            menu.close()
        elseif menu.key(keycode, pressed, mods, k) == "typed" then
            stage.stir(0.12)
        end
        stage.retarget()
        return true
    end
    if not mods.logo then
        -- In the overview the keys that choose work without the logo key.
        if stage.in_overview() and pressed and not (mods.logo or mods.alt or mods.ctrl) then
            local used = bound(keycode, mods)
            stage.retarget()
            return used
        end
        return false
    end
    if not pressed then
        -- The release of a key this script used (the core only passes those).
        return true
    end
    local used = bound(keycode, mods)
    stage.retarget()
    return used
end

-- The last time a held window was carried over the edge of the screen.
local carried_at = -math.huge

-- Whether the pointer is the launcher's (a click on it, or beside it).
local menu_has_pointer = false

function pointer_button(button, pressed, window, surface, mods)
    if menu_has_pointer or (menu.is_open() and pressed) then
        if pressed and button == BUTTON_LEFT and menu.is_open() then
            local x, y = hz.pointer()
            if x then
                menu.click(x, y)
            end
            menu_has_pointer = true
            stage.retarget()
            return "take"
        end
        if not pressed then
            menu_has_pointer = false
        end
        return true
    end
    if stage.held() then
        -- This script has the pointer (it answered "take" below) until
        -- every button is up; the core then gives it back to the windows.
        if not pressed and button == BUTTON_LEFT then
            stage.release()
        end
        return true
    end
    if stage.in_overview() then
        if pressed and button == BUTTON_LEFT then
            local x, y = hz.pointer()
            if x then
                local id = stage.window_at(x, y)
                local index = stage.workspace_at(x, y)
                if id then
                    space.focus(id)
                elseif index then
                    space.switch(index)
                end
                if id or index then
                    stage.set_overview(false)
                end
                stage.retarget()
            end
        end
        return true
    end
    -- With the logo key held, the left button picks a window up.
    if pressed and button == BUTTON_LEFT and mods and mods.logo then
        local x, y = hz.pointer()
        local id = x and stage.window_at(x, y)
        if id and not space.is_fullscreen(id) then
            space.focus(id)
            stage.retarget()
            if stage.hold(id, x, y) then
                return "take"
            end
        end
    end
    return false
end

-- The pointer moved while this script has it: a window is being carried.
function pointer_motion(x, y)
    local id = stage.held()
    if not id then
        return
    end
    stage.hold_to(x, y)
    -- Over another tile: the two change places.
    local other = stage.tile_at(x, y)
    if other and other ~= id and space.swap(id, other) then
        stage.stir(0.3)
    end
    -- Pushed against the left or right edge of the screen: on to the next
    -- workspace, the window in hand.
    local output = hz.outputs()[1]
    if output then
        local direction = 0
        if x <= output.x + 1 then
            direction = -1
        elseif x >= output.x + output.logical_width - 2 then
            direction = 1
        end
        local t = hz.time()
        if direction ~= 0 and t - carried_at > 0.6 then
            carried_at = t
            space.send(id, space.active() + direction, true)
        end
    end
    stage.retarget()
end

-- Three fingers sideways on the touchpad move the camera along the strip,
-- as far as the fingers go; lifted, it settles on the nearest workspace in
-- the direction it was going. Every other gesture is the application's.
local swipe = nil
-- Logical pixels of finger travel for one workspace.
local SWIPE_LENGTH = 420

function gesture_begin(g)
    if g.kind == "swipe" and g.fingers == 3 and not stage.in_overview() and not stage.held() then
        local at = stage.camera_now()
        swipe = { from = at, at = at, time = hz.time(), speed = 0 }
        stage.carry(at)
        return true
    end
    return false
end

function gesture_update(g)
    if not swipe or g.kind ~= "swipe" then
        return
    end
    local count = space.count()
    local at = math.clamp(swipe.at - g.dx / SWIPE_LENGTH, 0.8, count + 0.2)
    local t = hz.time()
    local dt = t - swipe.time
    if dt > 0 then
        -- Smoothed: the last few updates decide the speed at release.
        swipe.speed = swipe.speed * 0.5 + ((at - swipe.at) / dt) * 0.5
    end
    swipe.at, swipe.time = at, t
    stage.carry(at)
end

function gesture_end(g)
    if not swipe then
        return
    end
    local s = swipe
    swipe = nil
    local target = math.floor(s.from + 0.5)
    if not g.cancelled then
        -- Where it would coast to, but no further than one workspace on.
        target = math.floor(s.at + s.speed * 0.15 + 0.5)
        target = math.clamp(target, math.floor(s.at), math.ceil(s.at))
    end
    space.switch(target)
    stage.carry(nil, if g.cancelled then 0 else s.speed)
    stage.retarget()
end

-- Hooks for tools and tests, reached through the control interface
-- (`script_call`).

-- The launcher: whether it is open, what is typed, the rows in view.
function fancy_launcher()
    return menu.describe()
end

-- The workspaces: which is in view, and the windows of each in order.
function fancy_spaces()
    return space.describe()
end

-- What the last frame showed: the camera, and every window's place on
-- screen, opacity and heat.
function fancy_state()
    return stage.describe()
end

-- Changes one of the stage's settings (`stage.settings`), for trying
-- things out: `script-call fancy_setting '"cold_brightness"' 1`.
function fancy_setting(name, value)
    if stage.settings[name] == nil or type(stage.settings[name]) ~= type(value) then
        error("no such setting, or not that kind of value: " .. tostring(name))
    end
    stage.settings[name] = value
    hz.request_frame()
    return value
end

function fancy_bench(count)
    stage.bench(count)
end
