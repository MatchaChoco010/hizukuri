-- Example 5: many windows in one draw.
--
-- The texture of each quad is chosen by an index that travels in the instance
-- buffer, so the shader, not the script, decides which heap slot is read. The
-- shader declares a fixed-size texture array; the API binds it to consecutive
-- heap slots, checks every one of them, and the shader compiler clamps the
-- index to the array.

local SHADER = [[
enable wgpu_binding_array;

struct Push {
    target_size: vec2<f32>,
}
var<immediate> pc: Push;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) texture_index: u32,
}

@vertex
fn vs(
    @builtin(vertex_index) index: u32,
    @location(0) rect: vec4<f32>,
    @location(1) texture_index: u32,
) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0),
    );
    let corner = corners[index];
    let pixel = rect.xy + rect.zw * corner;
    var out: VsOut;
    out.position = vec4(pixel / pc.target_size * 2.0 - 1.0, 0.0, 1.0);
    out.uv = corner;
    out.texture_index = texture_index;
    return out;
}

@group(0) @binding(0) var window_textures: binding_array<texture_2d<f32>, 8>;
@group(0) @binding(1) var window_sampler: sampler;

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSampleLevel(window_textures[in.texture_index], window_sampler, in.uv, 0.0);
}
]]

local INSTANCE_BYTES = 20
local MAX_INSTANCES = 32768

local pipeline, sampler, instances
local instance_data = buffer.create(MAX_INSTANCES * INSTANCE_BYTES)
slots = nil

function init(textures)
    pipeline = gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", source = SHADER, entry = "vs" }),
        fragment = gfx.create_shader({ stage = "fragment", source = SHADER, entry = "fs" }),
        vertex_buffers = {
            {
                stride = INSTANCE_BYTES,
                step = "instance",
                attributes = {
                    { location = 0, format = "f32x4", offset = 0 },
                    { location = 1, format = "u32", offset = 16 },
                },
            },
        },
        targets = { { format = "rgba8unorm" } },
    })
    sampler = gfx.create_sampler({ mag = "nearest", min = "nearest" })
    instances = gfx.create_buffer({ size = MAX_INSTANCES * INSTANCE_BYTES, usage = { "vertex" } })
    -- Eight consecutive heap slots, one per texture.
    slots = gfx.create_texture_slots(textures)
end

-- `windows` is a list of { x, y, width, height, texture_index }.
function frame(target, windows)
    local width, height = gfx.image_info(target)
    for i, window in windows do
        local at = (i - 1) * INSTANCE_BYTES
        buffer.writef32(instance_data, at, window[1])
        buffer.writef32(instance_data, at + 4, window[2])
        buffer.writef32(instance_data, at + 8, window[3])
        buffer.writef32(instance_data, at + 12, window[4])
        buffer.writeu32(instance_data, at + 16, window[5])
    end
    gfx.write_buffer(instances, 0, instance_data)

    gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 0, 0, 0, 1 } } } })
    gfx.set_pipeline(pipeline)
    gfx.set_vertex_buffer(0, instances)
    gfx.bind(0, slots[1])
    gfx.bind(1, sampler)
    gfx.push_f32(0, width, height)
    gfx.draw(6, #windows)
    gfx.end_pass()
end

-- The same draw with the array bound to another slot, for the tests.
function frame_bound_to(target, first_slot)
    gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 0, 0, 0, 1 } } } })
    gfx.set_pipeline(pipeline)
    gfx.set_vertex_buffer(0, instances)
    gfx.bind(0, first_slot)
    gfx.bind(1, sampler)
    gfx.draw(6, 1)
    gfx.end_pass()
end

-- Benchmark: n quads on a grid, texture chosen per quad.
function frame_grid(target, n)
    local at = 0
    for i = 1, n do
        buffer.writef32(instance_data, at, (i % 64) * 4)
        buffer.writef32(instance_data, at + 4, (i // 64 % 64) * 4)
        buffer.writef32(instance_data, at + 8, 4)
        buffer.writef32(instance_data, at + 12, 4)
        buffer.writeu32(instance_data, at + 16, i % 8)
        at += INSTANCE_BYTES
    end
    gfx.write_buffer(instances, 0, instance_data)
    gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 0, 0, 0, 1 } } } })
    gfx.set_pipeline(pipeline)
    gfx.set_vertex_buffer(0, instances)
    gfx.bind(0, slots[1])
    gfx.bind(1, sampler)
    gfx.push_f32(0, 256, 256)
    gfx.draw(6, n)
    gfx.end_pass()
end
