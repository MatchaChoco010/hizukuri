-- The room: a desk against a wall, monitors on it, a few things around.
--
-- Everything is made here, from boxes and rectangles. The world is in
-- meters, y up, the viewer sits at +z and looks towards -z.
--
-- `S.monitors` is the list of screens. Each one is a rectangle in space
-- that shows a window:
--
--   name                 for the logs and the tests
--   center               the middle of the screen
--   right, up, normal    unit vectors of the screen (normal: towards the viewer)
--   width, height        size of the screen in meters
--   pixels_x, pixels_y   the resolution: the size windows on it are asked to have
--
-- `S.build()` returns the geometry as a list of quads:
--
--   { corners = { p1, p2, p3, p4 }, normal, material, layer }
--
-- with the corners counter-clockwise seen from the front. `layer` is not
-- used by the renderer (a depth buffer orders what is drawn); it says what
-- stands on what.

local m3 = hz.include("math3d")

local S = {}

-- Materials: albedo as linear sRGB (the renderer takes it to the working
-- space), roughness, reflectance
-- at normal incidence, and a kind the shader may give a pattern
-- (0: plain, 1: wood grain along x, 2: wall plaster, 3: carpet).
S.materials = {
    wall = { albedo = { 0.42, 0.41, 0.4 }, roughness = 0.9, f0 = 0.03, kind = 2 },
    floor = { albedo = { 0.045, 0.04, 0.04 }, roughness = 0.95, f0 = 0.02, kind = 3 },
    ceiling = { albedo = { 0.35, 0.35, 0.35 }, roughness = 0.95, f0 = 0.02, kind = 0 },
    desk = { albedo = { 0.16, 0.085, 0.045 }, roughness = 0.15, f0 = 0.05, kind = 1 },
    desk_frame = { albedo = { 0.02, 0.02, 0.022 }, roughness = 0.45, f0 = 0.5, kind = 0 },
    bezel = { albedo = { 0.012, 0.012, 0.014 }, roughness = 0.38, f0 = 0.045, kind = 0 },
    stand = { albedo = { 0.05, 0.05, 0.055 }, roughness = 0.3, f0 = 0.6, kind = 0 },
    keyboard = { albedo = { 0.02, 0.02, 0.022 }, roughness = 0.55, f0 = 0.04, kind = 0 },
    keycap = { albedo = { 0.07, 0.07, 0.075 }, roughness = 0.6, f0 = 0.04, kind = 0 },
    mat = { albedo = { 0.025, 0.027, 0.03 }, roughness = 0.8, f0 = 0.03, kind = 0 },
    mug = { albedo = { 0.42, 0.41, 0.4 }, roughness = 0.18, f0 = 0.05, kind = 0 },
    book_red = { albedo = { 0.26, 0.05, 0.04 }, roughness = 0.6, f0 = 0.04, kind = 0 },
    book_blue = { albedo = { 0.03, 0.08, 0.3 }, roughness = 0.6, f0 = 0.04, kind = 0 },
    book_cream = { albedo = { 0.4, 0.36, 0.28 }, roughness = 0.7, f0 = 0.04, kind = 0 },
    paper = { albedo = { 0.4, 0.4, 0.38 }, roughness = 0.8, f0 = 0.04, kind = 0 },
}

local DESK_TOP = 0.74
local DESK_HALF_WIDTH = 1.05
local DESK_FRONT = 0.5
local DESK_BACK = -0.44
local WALL_Z = -0.5

-- The top of the desk: its middle and half sizes.
S.desk = {
    center = { 0, DESK_TOP, (DESK_FRONT + DESK_BACK) / 2 },
    half_width = DESK_HALF_WIDTH,
    half_depth = (DESK_FRONT - DESK_BACK) / 2,
}

-- Screens. `yaw` turns a screen about the vertical axis (positive: its
-- face turns towards +x), `pitch` tips it (positive: its face turns down).
-- Angles in degrees. `mount`: "stand" (a foot on the desk) or "arm" (an
-- arm from the wall).
S.monitor_setup = {
    {
        name = "main",
        center = { 0, 1.045, -0.2 },
        yaw = 0,
        pitch = -3,
        width = 0.62,
        pixels = { 1920, 1080 },
        mount = "stand",
    },
    {
        name = "left",
        center = { -0.585, 1.02, -0.055 },
        yaw = 30,
        pitch = -3,
        width = 0.5,
        pixels = { 1280, 720 },
        mount = "stand",
    },
    {
        name = "right",
        center = { 0.515, 1.075, -0.08 },
        yaw = -28,
        pitch = -2,
        width = 0.3,
        pixels = { 720, 1280 },
        mount = "stand",
    },
    {
        name = "top",
        center = { 0.02, 1.47, -0.24 },
        yaw = 0,
        pitch = 14,
        width = 0.5,
        pixels = { 1280, 720 },
        mount = "arm",
    },
}

local function orientation(yaw, pitch)
    local qy = m3.quat_axis_angle({ 0, 1, 0 }, math.rad(yaw))
    local qp = m3.quat_axis_angle({ 1, 0, 0 }, math.rad(pitch))
    return m3.quat_mul(qy, qp)
end

S.monitors = {}

local function setup_monitors()
    S.monitors = {}
    for index, setup in S.monitor_setup do
        local rotation = orientation(setup.yaw, setup.pitch)
        local width = setup.width
        local height = width * setup.pixels[2] / setup.pixels[1]
        local normal = m3.quat_rotate(rotation, { 0, 0, 1 })
        -- How much of the screen's light reaches the desk in front of it
        -- (the share of the view from there that the screen fills).
        local to_desk = m3.sub({ setup.center[1] + normal[1] * 0.25, DESK_TOP, setup.center[3] + normal[3] * 0.25 }, setup.center)
        local distance2 = m3.dot(to_desk, to_desk)
        local toward = m3.scale(to_desk, 1 / math.sqrt(distance2))
        local onto_desk = math.max(m3.dot(normal, toward), 0) * math.max(-toward[2], 0) * width * height / (math.pi * distance2)
        table.insert(S.monitors, {
            index = index,
            name = setup.name,
            center = setup.center,
            rotation = rotation,
            right = m3.quat_rotate(rotation, { 1, 0, 0 }),
            up = m3.quat_rotate(rotation, { 0, 1, 0 }),
            normal = normal,
            onto_desk = math.min(onto_desk, 1),
            width = width,
            height = height,
            pixels_x = setup.pixels[1],
            pixels_y = setup.pixels[2],
            mount = setup.mount,
        })
    end
end

-- ----- geometry ---------------------------------------------------------

local quads

local function quad(p1, p2, p3, p4, material, layer)
    local normal = m3.normalize(m3.cross(m3.sub(p2, p1), m3.sub(p4, p1)))
    table.insert(quads, { corners = { p1, p2, p3, p4 }, normal = normal, material = material, layer = layer })
end

-- A box: `center`, half sizes `half`, turned by `rotation` (optional).
-- `skip` names faces to leave out ("+x", "-y", ...).
local function box(center, half, material, layer, rotation, skip)
    rotation = rotation or m3.quat_identity
    local function corner(sx, sy, sz)
        return m3.add(center, m3.quat_rotate(rotation, { sx * half[1], sy * half[2], sz * half[3] }))
    end
    local function face(name, a, b, c, d)
        if skip and skip[name] then
            return
        end
        quad(corner(a[1], a[2], a[3]), corner(b[1], b[2], b[3]), corner(c[1], c[2], c[3]), corner(d[1], d[2], d[3]), material, layer)
    end
    face("+z", { -1, -1, 1 }, { 1, -1, 1 }, { 1, 1, 1 }, { -1, 1, 1 })
    face("-z", { 1, -1, -1 }, { -1, -1, -1 }, { -1, 1, -1 }, { 1, 1, -1 })
    face("+x", { 1, -1, 1 }, { 1, -1, -1 }, { 1, 1, -1 }, { 1, 1, 1 })
    face("-x", { -1, -1, -1 }, { -1, -1, 1 }, { -1, 1, 1 }, { -1, 1, -1 })
    face("+y", { -1, 1, 1 }, { 1, 1, 1 }, { 1, 1, -1 }, { -1, 1, -1 })
    face("-y", { -1, -1, -1 }, { 1, -1, -1 }, { 1, -1, 1 }, { -1, -1, 1 })
end

-- What stands on the desk and keeps light off it around its foot:
-- { x, z, half_x, half_z, angle, height } (the footprint, turned by `angle`
-- radians about the vertical, and how tall the thing is). There are no
-- real shadows; the renderer darkens the desk close to these.
S.blockers = {}

local function stands_on_desk(center, half_x, half_z, angle, height)
    table.insert(S.blockers, { x = center[1], z = center[3], half_x = half_x, half_z = half_z, angle = angle, height = height })
end

-- A point given in a monitor's own axes (x right, y up, z towards the
-- viewer), from the middle of its screen.
local function on_monitor(monitor, x, y, z)
    return m3.add(monitor.center, m3.quat_rotate(monitor.rotation, { x, y, z }))
end

-- How far the frame stands out around the screen, how far it stands
-- proud of it, and how deep the body is.
local BEZEL = 0.009
local BEZEL_PROUD = 0.0025
local BODY_DEPTH = 0.028

local function monitor_body(monitor)
    local mat = S.materials
    local hw, hh = monitor.width / 2, monitor.height / 2
    local layer = 3
    -- The frame: four bars around the screen.
    local front = BEZEL_PROUD
    local mid_z = (front - BODY_DEPTH) / 2
    local half_z = (front + BODY_DEPTH) / 2
    box(on_monitor(monitor, 0, hh + BEZEL / 2, mid_z), { hw + BEZEL, BEZEL / 2, half_z }, mat.bezel, layer, monitor.rotation)
    box(on_monitor(monitor, 0, -hh - BEZEL, mid_z), { hw + BEZEL, BEZEL, half_z }, mat.bezel, layer, monitor.rotation)
    box(on_monitor(monitor, -hw - BEZEL / 2, -BEZEL / 2, mid_z), { BEZEL / 2, hh + BEZEL / 2, half_z }, mat.bezel, layer, monitor.rotation)
    box(on_monitor(monitor, hw + BEZEL / 2, -BEZEL / 2, mid_z), { BEZEL / 2, hh + BEZEL / 2, half_z }, mat.bezel, layer, monitor.rotation)
    -- The back of the body, behind the screen.
    box(
        on_monitor(monitor, 0, 0, -BODY_DEPTH / 2 - 0.002),
        { hw, hh, BODY_DEPTH / 2 - 0.002 },
        mat.bezel,
        layer - 1,
        monitor.rotation,
        { ["+z"] = true }
    )

    if monitor.mount == "stand" then
        -- A neck from the back of the body down to a foot on the desk.
        local back = on_monitor(monitor, 0, -hh * 0.2, -BODY_DEPTH - 0.02)
        local foot_y = DESK_TOP + 0.006
        local neck_top = back[2] + 0.05
        local neck_half = (neck_top - foot_y) / 2
        local yaw_only = m3.quat_axis_angle({ 0, 1, 0 }, math.atan2(monitor.normal[1], monitor.normal[3]))
        box({ back[1], foot_y + neck_half, back[3] }, { 0.03, neck_half, 0.012 }, mat.stand, layer - 2, yaw_only)
        -- The joint between the neck and the body.
        box(on_monitor(monitor, 0, -hh * 0.2, -BODY_DEPTH - 0.008), { 0.05, 0.05, 0.01 }, mat.stand, layer - 2, monitor.rotation)
        local foot = m3.add({ back[1], foot_y, back[3] }, m3.quat_rotate(yaw_only, { 0, 0, 0.06 }))
        box(foot, { math.min(0.13, hw * 0.6), 0.006, 0.1 }, mat.stand, layer - 2, yaw_only)
        stands_on_desk(foot, math.min(0.13, hw * 0.6), 0.1, math.atan2(monitor.normal[1], monitor.normal[3]), 0.03)
    else
        -- An arm from the wall.
        local back = on_monitor(monitor, 0, 0, -BODY_DEPTH - 0.012)
        local length = back[3] - WALL_Z
        box({ back[1], back[2], WALL_Z + length / 2 }, { 0.022, 0.022, length / 2 }, mat.stand, layer - 2)
        box({ back[1], back[2], WALL_Z + 0.006 }, { 0.06, 0.06, 0.006 }, mat.stand, layer - 2)
        box(on_monitor(monitor, 0, 0, -BODY_DEPTH - 0.006), { 0.05, 0.05, 0.008 }, mat.stand, layer - 2, monitor.rotation)
    end
end

local function yaw(degrees)
    return m3.quat_axis_angle({ 0, 1, 0 }, math.rad(degrees))
end

local function room_shell()
    local mat = S.materials
    local left, right, front, top = -1.7, 2.1, 2.8, 2.5
    -- Back wall, floor, side walls, the wall behind the viewer, ceiling.
    quad({ left, 0, WALL_Z }, { right, 0, WALL_Z }, { right, top, WALL_Z }, { left, top, WALL_Z }, mat.wall, 0)
    quad({ left, 0, front }, { right, 0, front }, { right, 0, WALL_Z }, { left, 0, WALL_Z }, mat.floor, 0)
    quad({ left, 0, front }, { left, 0, WALL_Z }, { left, top, WALL_Z }, { left, top, front }, mat.wall, 0)
    quad({ right, 0, WALL_Z }, { right, 0, front }, { right, top, front }, { right, top, WALL_Z }, mat.wall, 0)
    quad({ right, 0, front }, { left, 0, front }, { left, top, front }, { right, top, front }, mat.wall, 0)
    quad({ left, top, WALL_Z }, { right, top, WALL_Z }, { right, top, front }, { left, top, front }, mat.ceiling, 0)
    -- A skirting board.
    box({ (left + right) / 2, 0.04, WALL_Z + 0.006 }, { (right - left) / 2, 0.04, 0.006 }, mat.ceiling, 0, nil, { ["-z"] = true, ["-y"] = true })
end

local function desk()
    local mat = S.materials
    local depth = (DESK_FRONT - DESK_BACK) / 2
    local mid_z = (DESK_FRONT + DESK_BACK) / 2
    -- Legs and a rail, then the top.
    for _, x in { -DESK_HALF_WIDTH + 0.06, DESK_HALF_WIDTH - 0.06 } do
        box({ x, (DESK_TOP - 0.03) / 2, mid_z }, { 0.025, (DESK_TOP - 0.03) / 2, depth - 0.04 }, mat.desk_frame, 1)
    end
    box({ 0, DESK_TOP - 0.07, DESK_BACK + 0.05 }, { DESK_HALF_WIDTH - 0.085, 0.04, 0.012 }, mat.desk_frame, 1)
    box({ 0, DESK_TOP - 0.015, mid_z }, { DESK_HALF_WIDTH, 0.015, depth }, mat.desk, 2)
end

local function things()
    local mat = S.materials
    local y = DESK_TOP
    local layer = 4
    -- A keyboard and a mouse (on a small mat), straight on the wood: the
    -- desk in front of the screens stays free, to mirror them.
    box({ 0.3, y + 0.0015, 0.26 }, { 0.1, 0.0015, 0.11 }, mat.mat, layer)
    local keyboard = { -0.05, y + 0.009, 0.27 }
    box(keyboard, { 0.17, 0.009, 0.06 }, mat.keyboard, layer + 1)
    -- Rows of keys.
    for row = 0, 4 do
        local z = keyboard[3] - 0.045 + row * 0.0225
        local count = if row == 4 then 3 else 14
        for i = 0, count - 1 do
            local x, half
            if row == 4 then
                -- The bottom row: a space bar between two wide keys.
                x = keyboard[1] + ({ -0.11, 0, 0.11 })[i + 1]
                half = ({ 0.04, 0.062, 0.04 })[i + 1]
            else
                x = keyboard[1] - 0.1495 + i * 0.023
                half = 0.0085
            end
            box({ x, keyboard[2] + 0.009 + 0.003, z }, { half, 0.003, 0.0085 }, mat.keycap, layer + 2, nil, { ["-y"] = true })
        end
    end
    box({ 0.3, y + 0.003 + 0.014, 0.27 }, { 0.03, 0.014, 0.05 }, mat.keyboard, layer + 1, yaw(8))
    -- A mug: a white box, to catch the colors of the screens.
    box({ -0.52, y + 0.047, 0.24 }, { 0.038, 0.047, 0.038 }, mat.mug, layer + 1, yaw(20))
    box({ -0.52 + 0.048, y + 0.05, 0.24 - 0.017 }, { 0.012, 0.028, 0.007 }, mat.mug, layer + 1, yaw(20))
    -- A stack of books and a few sheets of paper.
    box({ 0.74, y + 0.016, 0.16 }, { 0.085, 0.016, 0.12 }, mat.book_red, layer + 1, yaw(-12))
    box({ 0.745, y + 0.032 + 0.011, 0.165 }, { 0.078, 0.011, 0.11 }, mat.book_cream, layer + 2, yaw(-4))
    box({ 0.735, y + 0.054 + 0.013, 0.16 }, { 0.07, 0.013, 0.1 }, mat.book_blue, layer + 3, yaw(-19))
    box({ -0.8, y + 0.002, 0.12 }, { 0.105, 0.002, 0.1485 }, mat.paper, layer, yaw(14))
    box({ -0.79, y + 0.005, 0.13 }, { 0.105, 0.001, 0.1485 }, mat.paper, layer + 1, yaw(5))

    stands_on_desk(keyboard, 0.17, 0.06, 0, 0.024)
    stands_on_desk({ 0.3, y, 0.27 }, 0.03, 0.05, math.rad(8), 0.03)
    stands_on_desk({ -0.52, y, 0.24 }, 0.038, 0.038, math.rad(20), 0.08)
    stands_on_desk({ 0.74, y, 0.16 }, 0.085, 0.12, math.rad(-12), 0.07)
end

-- Builds the monitors (`S.monitors`) and returns the quads of the room.
function S.build()
    setup_monitors()
    S.blockers = {}
    quads = {}
    room_shell()
    desk()
    for _, monitor in S.monitors do
        monitor_body(monitor)
    end
    things()
    local result = quads
    quads = nil
    return result
end

-- What a view of the room has to show: the corners of every monitor (with
-- its frame), and the front of the desk, where the screens are mirrored.
function S.view_points()
    local points = {}
    for _, monitor in S.monitors do
        local hw, hh = monitor.width / 2 + 2 * BEZEL, monitor.height / 2 + 2 * BEZEL
        table.insert(points, on_monitor(monitor, -hw, hh, 0))
        table.insert(points, on_monitor(monitor, hw, hh, 0))
        table.insert(points, on_monitor(monitor, -hw, -hh, 0))
        table.insert(points, on_monitor(monitor, hw, -hh, 0))
    end
    table.insert(points, { -0.78, DESK_TOP, DESK_FRONT - 0.04 })
    table.insert(points, { 0.78, DESK_TOP, DESK_FRONT - 0.04 })
    return points
end

return S
