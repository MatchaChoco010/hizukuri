-- Damped springs, solved exactly.
--
-- A spring pulls a value towards a target. Its state at any time is known
-- in closed form from the state it had when it was last aimed, so nothing
-- is integrated step by step: a frame that comes late, or a clock that a
-- test moves by hand, gives the same motion.
--
-- Aiming a spring somewhere else keeps its value *and its speed*: a window
-- that is on its way does not stop to turn round.
--
--   stiffness `omega`: the natural frequency in radians per second. The
--     value is within 5 % of the target after about 4.7 / omega seconds
--     (critically damped).
--   damping `zeta`: 1 arrives without overshoot; below 1 it swings past
--     and comes back.

local S = {}

-- A spring at rest at `value`. `epsilon` is the distance below which it
-- counts as arrived.
function S.new(value, omega, zeta, epsilon)
    return {
        target = value,
        -- Offset from the target and speed at time `t0`.
        y0 = 0,
        v0 = 0,
        t0 = 0,
        omega = omega,
        zeta = math.min(zeta or 1, 1),
        epsilon = epsilon or 0.01,
        resting = true,
    }
end

-- The value and the speed at time t.
function S.at(s, t)
    if s.resting then
        return s.target, 0
    end
    local dt = math.max(t - s.t0, 0)
    local w, z = s.omega, s.zeta
    local y, v
    if z >= 1 then
        local e = math.exp(-w * dt)
        local c = s.v0 + w * s.y0
        y = e * (s.y0 + c * dt)
        v = e * (s.v0 - w * c * dt)
    else
        local a = z * w
        local b = w * math.sqrt(1 - z * z)
        local e = math.exp(-a * dt)
        local cos, sin = math.cos(b * dt), math.sin(b * dt)
        local k = (s.v0 + a * s.y0) / b
        y = e * (s.y0 * cos + k * sin)
        v = e * (s.v0 * cos - (a * k + b * s.y0) * sin)
    end
    return s.target + y, v
end

-- Aims the spring at `target` from time t on, keeping value and speed.
function S.aim(s, target, t)
    if s.resting then
        if target == s.target then
            return
        end
        s.y0 = s.target - target
        s.v0 = 0
    else
        local x, v = S.at(s, t)
        s.y0 = x - target
        s.v0 = v
    end
    s.target = target
    s.t0 = t
    s.resting = false
end

-- Puts the spring at `value`, at rest.
function S.snap(s, value)
    s.target = value
    s.y0 = 0
    s.v0 = 0
    s.resting = true
end

-- Puts the spring at `value` at time t, still aimed where it was, with
-- speed `speed` (default: none).
function S.place(s, value, t, speed)
    s.y0 = value - s.target
    s.v0 = speed or 0
    s.t0 = t
    s.resting = false
end

-- Samples the spring at time t and lets it come to rest once it is close
-- and slow enough. Returns the value, the speed, and whether it still moves.
function S.step(s, t)
    if s.resting then
        return s.target, 0, false
    end
    local x, v = S.at(s, t)
    if math.abs(x - s.target) < s.epsilon and math.abs(v) < s.epsilon * s.omega then
        s.y0 = 0
        s.v0 = 0
        s.resting = true
        return s.target, 0, false
    end
    return x, v, true
end

return S
