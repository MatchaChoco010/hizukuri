-- 3D math for the room: vectors, quaternions, 4x4 matrices, rays.
--
-- Everything is in double precision (plain tables of numbers), because the
-- same numbers decide where a pointer lands inside a window. The world is
-- right handed with y up, in meters. A camera looks along its own -z.
--
--   vec3: { x, y, z }
--   quat: { x, y, z, w }, a rotation
--   mat4: 16 numbers, row major: m[(row - 1) * 4 + column]. A point p is
--         transformed as m * p (column vectors).

local M = {}

-- ----- vec3 --------------------------------------------------------------

function M.vec3(x, y, z)
    return { x, y, z }
end

function M.add(a, b)
    return { a[1] + b[1], a[2] + b[2], a[3] + b[3] }
end

function M.sub(a, b)
    return { a[1] - b[1], a[2] - b[2], a[3] - b[3] }
end

function M.scale(a, s)
    return { a[1] * s, a[2] * s, a[3] * s }
end

-- a + b * s
function M.madd(a, b, s)
    return { a[1] + b[1] * s, a[2] + b[2] * s, a[3] + b[3] * s }
end

function M.dot(a, b)
    return a[1] * b[1] + a[2] * b[2] + a[3] * b[3]
end

function M.cross(a, b)
    return {
        a[2] * b[3] - a[3] * b[2],
        a[3] * b[1] - a[1] * b[3],
        a[1] * b[2] - a[2] * b[1],
    }
end

function M.length(a)
    return math.sqrt(a[1] * a[1] + a[2] * a[2] + a[3] * a[3])
end

function M.normalize(a)
    local l = M.length(a)
    if l == 0 then
        return { 0, 0, 0 }
    end
    return { a[1] / l, a[2] / l, a[3] / l }
end

function M.lerp(a, b, t)
    return {
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    }
end

-- ----- quaternions ------------------------------------------------------

M.quat_identity = { 0, 0, 0, 1 }

-- The rotation by `angle` radians about the unit vector `axis`.
function M.quat_axis_angle(axis, angle)
    local s = math.sin(angle / 2)
    return { axis[1] * s, axis[2] * s, axis[3] * s, math.cos(angle / 2) }
end

-- a * b: first b, then a.
function M.quat_mul(a, b)
    return {
        a[4] * b[1] + a[1] * b[4] + a[2] * b[3] - a[3] * b[2],
        a[4] * b[2] - a[1] * b[3] + a[2] * b[4] + a[3] * b[1],
        a[4] * b[3] + a[1] * b[2] - a[2] * b[1] + a[3] * b[4],
        a[4] * b[4] - a[1] * b[1] - a[2] * b[2] - a[3] * b[3],
    }
end

function M.quat_normalize(q)
    local l = math.sqrt(q[1] * q[1] + q[2] * q[2] + q[3] * q[3] + q[4] * q[4])
    return { q[1] / l, q[2] / l, q[3] / l, q[4] / l }
end

-- Rotates a vector.
function M.quat_rotate(q, v)
    -- v + 2 * cross(q.xyz, cross(q.xyz, v) + q.w * v)
    local qx, qy, qz, qw = q[1], q[2], q[3], q[4]
    local tx = qy * v[3] - qz * v[2] + qw * v[1]
    local ty = qz * v[1] - qx * v[3] + qw * v[2]
    local tz = qx * v[2] - qy * v[1] + qw * v[3]
    return {
        v[1] + 2 * (qy * tz - qz * ty),
        v[2] + 2 * (qz * tx - qx * tz),
        v[3] + 2 * (qx * ty - qy * tx),
    }
end

-- The rotation whose local axes are the orthonormal vectors `right`, `up`
-- and `back` (x, y and z).
function M.quat_from_axes(right, up, back)
    local m00, m01, m02 = right[1], up[1], back[1]
    local m10, m11, m12 = right[2], up[2], back[2]
    local m20, m21, m22 = right[3], up[3], back[3]
    local trace = m00 + m11 + m22
    local q
    if trace > 0 then
        local s = math.sqrt(trace + 1) * 2
        q = { (m21 - m12) / s, (m02 - m20) / s, (m10 - m01) / s, s / 4 }
    elseif m00 > m11 and m00 > m22 then
        local s = math.sqrt(1 + m00 - m11 - m22) * 2
        q = { s / 4, (m01 + m10) / s, (m02 + m20) / s, (m21 - m12) / s }
    elseif m11 > m22 then
        local s = math.sqrt(1 + m11 - m00 - m22) * 2
        q = { (m01 + m10) / s, s / 4, (m12 + m21) / s, (m02 - m20) / s }
    else
        local s = math.sqrt(1 + m22 - m00 - m11) * 2
        q = { (m02 + m20) / s, (m12 + m21) / s, s / 4, (m10 - m01) / s }
    end
    return M.quat_normalize(q)
end

-- The rotation of a camera at `eye` that looks at `target` (it looks along
-- its own -z), with `up` as the rough up direction.
function M.quat_look_at(eye, target, up)
    local back = M.normalize(M.sub(eye, target))
    local right = M.normalize(M.cross(up, back))
    local true_up = M.cross(back, right)
    return M.quat_from_axes(right, true_up, back)
end

-- Spherical interpolation, the short way round.
function M.quat_slerp(a, b, t)
    local d = a[1] * b[1] + a[2] * b[2] + a[3] * b[3] + a[4] * b[4]
    local sign = 1
    if d < 0 then
        d = -d
        sign = -1
    end
    local ka, kb
    if d > 0.9995 then
        ka, kb = 1 - t, t
    else
        local theta = math.acos(d)
        local s = math.sin(theta)
        ka, kb = math.sin((1 - t) * theta) / s, math.sin(t * theta) / s
    end
    kb *= sign
    return M.quat_normalize({
        a[1] * ka + b[1] * kb,
        a[2] * ka + b[2] * kb,
        a[3] * ka + b[3] * kb,
        a[4] * ka + b[4] * kb,
    })
end

-- ----- mat4 -------------------------------------------------------------

function M.mat4_identity()
    return { 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1 }
end

function M.mat4_mul(a, b)
    local m = table.create(16)
    for r = 0, 3 do
        local a1, a2, a3, a4 = a[r * 4 + 1], a[r * 4 + 2], a[r * 4 + 3], a[r * 4 + 4]
        for c = 1, 4 do
            m[r * 4 + c] = a1 * b[c] + a2 * b[4 + c] + a3 * b[8 + c] + a4 * b[12 + c]
        end
    end
    return m
end

-- World to view, for a camera at `eye` turned by `rotation`.
function M.mat4_view(eye, rotation)
    local right = M.quat_rotate(rotation, { 1, 0, 0 })
    local up = M.quat_rotate(rotation, { 0, 1, 0 })
    local back = M.quat_rotate(rotation, { 0, 0, 1 })
    return {
        right[1], right[2], right[3], -M.dot(right, eye),
        up[1], up[2], up[3], -M.dot(up, eye),
        back[1], back[2], back[3], -M.dot(back, eye),
        0, 0, 0, 1,
    }
end

-- View to clip, for Vulkan: y points down, depth runs from 0 at `near` to
-- 1 at `far`. `focal` is 1 / tan(vertical field of view / 2).
function M.mat4_perspective(focal, aspect, near, far)
    return {
        focal / aspect, 0, 0, 0,
        0, -focal, 0, 0,
        0, 0, far / (near - far), near * far / (near - far),
        0, 0, -1, 0,
    }
end

-- Transforms a point; returns x, y, z, w of the result.
function M.mat4_transform(m, p)
    local x, y, z = p[1], p[2], p[3]
    return m[1] * x + m[2] * y + m[3] * z + m[4],
        m[5] * x + m[6] * y + m[7] * z + m[8],
        m[9] * x + m[10] * y + m[11] * z + m[12],
        m[13] * x + m[14] * y + m[15] * z + m[16]
end

-- ----- rays -------------------------------------------------------------

-- The ray from a camera through a point of the screen. `nx`, `ny` are the
-- point in normalized device coordinates (-1..1, y down, as in Vulkan).
-- Returns the origin and the direction (not normalized; its length along
-- the view axis is 1).
function M.camera_ray(eye, rotation, focal, aspect, nx, ny)
    local direction = M.quat_rotate(rotation, { nx * aspect / focal, -ny / focal, -1 })
    return eye, direction
end

-- Where a ray meets the rectangle centered at `center`, spanned by the unit
-- vectors `right` and `up`, of size `width` x `height`. Returns the
-- distance along the ray and the point in the rectangle as u, v (0..1 from
-- its left, 0..1 from its top), or nil when the ray misses, runs parallel,
-- or comes from behind.
function M.ray_rectangle(origin, direction, center, right, up, width, height)
    local normal = M.cross(right, up)
    local facing = M.dot(direction, normal)
    if facing >= -1e-9 then
        return nil
    end
    local t = M.dot(M.sub(center, origin), normal) / facing
    if t <= 0 then
        return nil
    end
    local offset = M.sub(M.madd(origin, direction, t), center)
    local u = M.dot(offset, right) / width + 0.5
    local v = 0.5 - M.dot(offset, up) / height
    if u < 0 or u > 1 or v < 0 or v > 1 then
        return nil
    end
    return t, u, v
end

return M
