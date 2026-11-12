-- Example 4: a compute pass (box blur), then a render pass that samples its
-- result.

local BLUR = [[
struct Push {
    radius: i32,
}
var<immediate> pc: Push;

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var destination: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = vec2<i32>(textureDimensions(source));
    let p = vec2<i32>(id.xy);
    if p.x >= size.x || p.y >= size.y {
        return;
    }
    var sum = vec4(0.0);
    for (var dy = -pc.radius; dy <= pc.radius; dy++) {
        for (var dx = -pc.radius; dx <= pc.radius; dx++) {
            let q = clamp(p + vec2(dx, dy), vec2(0), size - 1);
            sum += textureLoad(source, q, 0);
        }
    }
    let side = f32(2 * pc.radius + 1);
    textureStore(destination, p, sum / (side * side));
}
]]

local PRESENT = [[
@group(0) @binding(0) var image: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    var corners = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(corners[index], 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(image, vec2<i32>(position.xy), 0);
}
]]

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
    blur = gfx.create_compute_pipeline(gfx.create_shader({ stage = "compute", source = BLUR }))
    present = gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", source = PRESENT, entry = "vs" }),
        fragment = gfx.create_shader({ stage = "fragment", source = PRESENT, entry = "fs" }),
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
