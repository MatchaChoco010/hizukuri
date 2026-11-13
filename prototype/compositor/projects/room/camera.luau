-- The camera, and how it moves.
--
-- A pose is { eye, rotation, focal, room }: where the camera is, how it is
-- turned (a quaternion; it looks along its own -z), 1 / tan(half the
-- vertical field of view), and how much it is a view of the room (1) or of
-- one screen that fills the picture (0).
--
-- The camera is always on its way from one pose to another: a start pose,
-- an end pose, a start time and a duration. The pose at time t is computed
-- from those alone, so a frame for a given time always looks the same.
-- When the goal changes, the new move starts from the pose at that moment.

local m3 = hz.include("math3d")

local C = {}

C.settings = {
    -- Seconds the camera takes to get to a new pose.
    duration = 0.55,
    -- 1 / tan(vertical field of view / 2). 2.4 is about 45 degrees.
    focal = 2.4,
    near = 0.05,
    far = 30,
    -- The view of the room: the point the camera looks at, and the
    -- direction from there to the chair. How far away the chair is follows
    -- from what has to be in the picture (`C.room_pose`).
    look_at = { 0, 1.07, -0.2 },
    toward_chair = { 0, 0.3, 1.8 },
    -- How far the look-at point follows the focused monitor (0: not at
    -- all, 1: to its center), and how far the chair moves sideways with it.
    look_follow = 0.3,
    side_follow = 0.5,
    -- The part of the picture left free around what has to be in it.
    margin = 0.05,
}

local function ease(t)
    -- ease in-out cubic
    if t < 0.5 then
        return 4 * t * t * t
    end
    local u = 2 - 2 * t
    return 1 - u * u * u / 2
end

-- The pose from which a monitor fills the output exactly, seen straight
-- on. `aspect` is the output's width / height.
function C.full_pose(monitor, aspect)
    local focal = C.settings.focal
    local distance = math.max(focal * monitor.height / 2, focal * monitor.width / (2 * aspect))
    return {
        eye = m3.madd(monitor.center, monitor.normal, distance),
        rotation = m3.quat_from_axes(monitor.right, monitor.up, monitor.normal),
        focal = focal,
        room = 0,
    }
end

-- The pose for looking at the room with a monitor focused: from the chair,
-- turned a little towards that monitor, as close as keeps every one of
-- `points` (the corners of what has to be seen) in a picture of the given
-- aspect.
function C.room_pose(monitor, aspect, points)
    local s = C.settings
    local look = s.look_at
    local toward = s.toward_chair
    if monitor then
        local to_monitor = m3.sub(monitor.center, s.look_at)
        look = m3.madd(look, to_monitor, s.look_follow)
        toward = { toward[1] + to_monitor[1] * s.side_follow, toward[2], toward[3] }
    end
    local back = m3.normalize(toward)
    local rotation = m3.quat_look_at(m3.add(look, back), look, { 0, 1, 0 })
    local right = m3.quat_rotate(rotation, { 1, 0, 0 })
    local up = m3.quat_rotate(rotation, { 0, 1, 0 })
    -- A point at (x, y) across and z towards the camera is inside the
    -- picture from the distance d when |x| * focal / aspect and |y| * focal
    -- are at most (1 - margin) * (d - z).
    local reach = 1 - s.margin
    local distance = 0.3
    for _, point in points do
        local offset = m3.sub(point, look)
        local x, y, z = m3.dot(offset, right), m3.dot(offset, up), m3.dot(offset, back)
        distance = math.max(distance, z + math.abs(x) * s.focal / (aspect * reach), z + math.abs(y) * s.focal / reach)
    end
    return {
        eye = m3.madd(look, back, distance),
        rotation = rotation,
        focal = s.focal,
        room = 1,
    }
end

local function blend(a, b, k)
    return {
        eye = m3.lerp(a.eye, b.eye, k),
        rotation = m3.quat_slerp(a.rotation, b.rotation, k),
        focal = a.focal + (b.focal - a.focal) * k,
        room = a.room + (b.room - a.room) * k,
    }
end

-- A pose part of the way (k: 0..1) from `a` to `b`.
C.between = blend

-- The move in progress.
local move = nil

-- The pose at time t, and whether the camera is still moving.
function C.sample(t)
    if not move then
        return nil, false
    end
    local p = if move.duration > 0 then (t - move.start) / move.duration else 1
    if p >= 1 then
        return move.to, false
    end
    return blend(move.from, move.to, ease(math.max(p, 0))), true
end

-- Sends the camera to `pose`, starting at time t from where it is then.
-- Without a move in progress (the first call), it is there at once.
function C.go(pose, t, duration)
    if not move then
        move = { from = pose, to = pose, start = t, duration = 0 }
        return
    end
    local from = C.sample(t)
    move = { from = from, to = pose, start = t, duration = duration or C.settings.duration }
end

function C.reset()
    move = nil
end

-- World to clip for a pose.
function C.view_projection(pose, aspect)
    local s = C.settings
    return m3.mat4_mul(m3.mat4_perspective(pose.focal, aspect, s.near, s.far), m3.mat4_view(pose.eye, pose.rotation))
end

return C
