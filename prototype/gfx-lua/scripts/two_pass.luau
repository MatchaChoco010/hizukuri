-- Example 3: two passes.
--   1. Composite windows into a linear RGBA16F target (premultiplied alpha).
--   2. Encode that target with the output transfer function, in a shader.
-- Per-frame parameters travel in a uniform buffer.

local composite, sampler
local encoders = {} -- output format -> pipeline
local encode_vs, encode_fs
local globals_buffer, globals_slot
local globals_data = buffer.create(16)
local linear_image, linear_slot, linear_width, linear_height
local slots = {}

function init()
    composite = gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", shader = "shaders/two_pass_composite.slang", entry = "composite_vs" }),
        fragment = gfx.create_shader({ stage = "fragment", shader = "shaders/two_pass_composite.slang", entry = "composite_fs" }),
        targets = { { format = "rgba16float", blend = "premultiplied" } },
    })
    encode_vs = gfx.create_shader({ stage = "vertex", shader = "shaders/two_pass_encode.slang", entry = "encode_vs" })
    encode_fs = gfx.create_shader({ stage = "fragment", shader = "shaders/two_pass_encode.slang", entry = "encode_fs" })
    sampler = gfx.create_sampler({ mag = "nearest", min = "nearest" })
    globals_buffer = gfx.create_buffer({ size = 16, usage = { "uniform" } })
    globals_slot = gfx.create_uniform_slot(globals_buffer, 0, 16)
end

local function encoder_for(format)
    local pipeline = encoders[format]
    if not pipeline then
        pipeline = gfx.create_render_pipeline({
            vertex = encode_vs,
            fragment = encode_fs,
            targets = { { format = format } },
        })
        encoders[format] = pipeline
    end
    return pipeline
end

local function linear_target_for(width, height)
    if linear_image and (linear_width ~= width or linear_height ~= height) then
        gfx.destroy(linear_slot)
        gfx.destroy(linear_image)
        linear_image = nil
    end
    if not linear_image then
        linear_image = gfx.create_image({
            format = "rgba16float",
            width = width,
            height = height,
            usage = { "color_target", "sampled", "copy_src" },
        })
        linear_slot = gfx.create_texture_slot(linear_image)
        linear_width, linear_height = width, height
    end
    return linear_image
end

-- `mode`: 0 encodes sRGB, 1 encodes PQ. Returns the linear image so that the
-- test can read both stages back.
function frame(output, windows, mode, exposure)
    local width, height, format = gfx.image_info(output)
    local linear = linear_target_for(width, height)

    buffer.writef32(globals_data, 0, exposure)
    buffer.writeu32(globals_data, 4, mode)
    buffer.writef32(globals_data, 8, 100.0)
    gfx.write_buffer(globals_buffer, 0, globals_data)

    gfx.begin_pass({ color = { { image = linear, load = "clear", clear = { 0.25, 0.25, 0.25, 1 } } } })
    gfx.set_pipeline(composite)
    gfx.bind(1, sampler)
    for _, window in windows do
        local slot = slots[window.image]
        if not slot then
            slot = gfx.create_texture_slot(window.image)
            slots[window.image] = slot
        end
        gfx.push_f32(0, window.x, window.y, window.width, window.height, width, height)
        gfx.bind(0, slot)
        gfx.draw(6)
    end
    gfx.end_pass()

    gfx.begin_pass({ color = { { image = output, load = "dont-care" } } })
    gfx.set_pipeline(encoder_for(format))
    gfx.bind(0, linear_slot)
    gfx.bind(2, globals_slot)
    gfx.draw(3)
    gfx.end_pass()

    last_linear_image = linear
end
