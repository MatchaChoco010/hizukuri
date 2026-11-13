-- Example 2: textures handed in from outside (stand-ins for window contents)
-- are drawn as quads. The script owns the layout and changes it every frame.

local pipeline, sampler
local slots = {} -- image handle -> heap slot

function init()
    local vs = gfx.create_shader({ stage = "vertex", shader = "shaders/windows.slang", entry = "vs" })
    local fs = gfx.create_shader({ stage = "fragment", shader = "shaders/windows.slang", entry = "fs" })
    pipeline = gfx.create_render_pipeline({
        vertex = vs,
        fragment = fs,
        targets = { { format = "rgba8unorm" } },
    })
    sampler = gfx.create_sampler({ mag = "nearest", min = "nearest" })
end

-- The layout policy. The test recomputes the same numbers.
local ORIGIN = vector.create(8, 8, 0)
local SPACING = vector.create(40, 6, 0)
local VELOCITY = vector.create(4, 2, 0)

function frame(target, windows, tick)
    local width, height = gfx.image_info(target)
    gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 0, 0, 0, 1 } } } })
    gfx.set_pipeline(pipeline)
    gfx.bind(1, sampler)
    for i, window in windows do
        local slot = slots[window.image]
        if not slot then
            slot = gfx.create_texture_slot(window.image)
            slots[window.image] = slot
        end
        -- Vector arithmetic on Luau's native vector type: no allocation.
        local position = ORIGIN + SPACING * (i - 1) + VELOCITY * tick
        local cos, sin = 1, 0
        if window.quarter_turn then
            cos, sin = 0, 1
        end
        gfx.push_f32(0, position.x, position.y, window.width, window.height, width, height, cos, sin, 1, 1, 1, 1)
        gfx.bind(0, slot)
        gfx.draw(6)
    end
    gfx.end_pass()
end
