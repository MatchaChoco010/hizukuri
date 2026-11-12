-- 2D affine transforms.
--
-- A transform is { a, b, c, d, tx, ty } and maps (x, y) to
-- (a * x + b * y + tx, c * x + d * y + ty).

local A = {}

function A.identity()
    return { 1, 0, 0, 1, 0, 0 }
end

function A.translate(tx, ty)
    return { 1, 0, 0, 1, tx, ty }
end

function A.scale(sx, sy)
    return { sx, 0, 0, sy or sx, 0, 0 }
end

-- Rotation by `radians`. With y pointing down, positive angles turn
-- clockwise on screen.
function A.rotate(radians)
    local c, s = math.cos(radians), math.sin(radians)
    return { c, -s, s, c, 0, 0 }
end

-- Shear: x moves by kx per unit of y, y moves by ky per unit of x.
function A.shear(kx, ky)
    return { 1, kx, ky or 0, 1, 0, 0 }
end

-- The transform that applies `n` first, then `m`.
function A.multiply(m, n)
    return {
        m[1] * n[1] + m[2] * n[3],
        m[1] * n[2] + m[2] * n[4],
        m[3] * n[1] + m[4] * n[3],
        m[3] * n[2] + m[4] * n[4],
        m[1] * n[5] + m[2] * n[6] + m[5],
        m[3] * n[5] + m[4] * n[6] + m[6],
    }
end

-- Chains transforms: the last argument is applied first.
function A.chain(first, ...)
    local result = first
    for _, m in { ... } do
        result = A.multiply(result, m)
    end
    return result
end

-- `m` applied around the point (cx, cy) instead of the origin.
function A.about(cx, cy, m)
    return A.chain(A.translate(cx, cy), m, A.translate(-cx, -cy))
end

-- The inverse, or nil when the transform flattens the plane.
function A.invert(m)
    local det = m[1] * m[4] - m[2] * m[3]
    if math.abs(det) < 1e-12 then
        return nil
    end
    local a, b, c, d = m[4] / det, -m[2] / det, -m[3] / det, m[1] / det
    return { a, b, c, d, -(a * m[5] + b * m[6]), -(c * m[5] + d * m[6]) }
end

function A.apply(m, x, y)
    return m[1] * x + m[2] * y + m[5], m[3] * x + m[4] * y + m[6]
end

return A
