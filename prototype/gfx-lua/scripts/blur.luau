-- Example 4: a compute pass (box blur), then a render pass that samples its
-- result.

local blur, present
local push_data = buffer.create(4)
local slots = {}

local function slot_for(key, create, image)
    local slot = slots[key]
    if not slot then
        slot = create(image)
        slots[key] = slot
    end
    return slot
end

function init()
    blur = gfx.create_compute_pipeline(gfx.create_shader({ stage = "compute", shader = "shaders/blur_compute.slang", entry = "blur" }))
    present = gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", shader = "shaders/blur_present.slang", entry = "present_vs" }),
        fragment = gfx.create_shader({ stage = "fragment", shader = "shaders/blur_present.slang", entry = "present_fs" }),
        targets = { { format = "rgba8unorm" } },
    })
end

function frame(source, blurred, output, radius)
    local width, height = gfx.image_info(source)

    gfx.set_pipeline(blur)
    gfx.bind(0, slot_for("src", gfx.create_texture_slot, source))
    gfx.bind(1, slot_for("dst", gfx.create_storage_texture_slot, blurred))
    buffer.writei32(push_data, 0, radius)
    gfx.push(0, push_data)
    gfx.dispatch(math.ceil(width / 8), math.ceil(height / 8))

    gfx.begin_pass({ color = { { image = output, load = "dont-care" } } })
    gfx.set_pipeline(present)
    gfx.bind(0, slot_for("blurred", gfx.create_texture_slot, blurred))
    gfx.draw(3)
    gfx.end_pass()
end
