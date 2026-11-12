-- Example 2: textures handed in from outside (stand-ins for window contents)
-- are drawn as quads. The script owns the layout and changes it every frame.

local SHADER = [[
struct Push {
    rect: vec4<f32>,        // x, y, width, height in target pixels
    target_size: vec2<f32>,
    rotation: vec2<f32>,    // cos, sin
    tint: vec4<f32>,
}
var<immediate> pc: Push;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs(@builtin(vertex_index) index: u32) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0),
    );
    let corner = corners[index];
    let local = (corner - 0.5) * pc.rect.zw;
    let rotated = vec2(
        local.x * pc.rotation.x - local.y * pc.rotation.y,
        local.x * pc.rotation.y + local.y * pc.rotation.x,
    );
    let pixel = pc.rect.xy + pc.rect.zw * 0.5 + rotated;
    var out: VsOut;
    out.position = vec4(pixel / pc.target_size * 2.0 - 1.0, 0.0, 1.0);
    out.uv = corner;
    return out;
}

@group(0) @binding(0) var window_texture: texture_2d<f32>;
@group(0) @binding(1) var window_sampler: sampler;

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(window_texture, window_sampler, in.uv) * pc.tint;
}
]]

local pipeline, sampler
local slots = {} -- image handle -> heap slot

function init()
    local vs = gfx.create_shader({ stage = "vertex", source = SHADER, entry = "vs" })
    local fs = gfx.create_shader({ stage = "fragment", source = SHADER, entry = "fs" })
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
