-- Example 1: a script builds a pipeline from nothing and draws a triangle.
-- Vertex and index data are assembled in a Luau `buffer`.

local SHADER = [[
struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs(@location(0) position: vec2<f32>, @location(1) color: vec4<f32>) -> VsOut {
    var out: VsOut;
    out.position = vec4(position, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
]]

local pipeline, vertices, indices

function init()
    local vs = gfx.create_shader({ stage = "vertex", source = SHADER, entry = "vs" })
    local fs = gfx.create_shader({ stage = "fragment", source = SHADER, entry = "fs" })
    pipeline = gfx.create_render_pipeline({
        vertex = vs,
        fragment = fs,
        vertex_buffers = {
            {
                stride = 24,
                attributes = {
                    { location = 0, format = "f32x2", offset = 0 },
                    { location = 1, format = "f32x4", offset = 8 },
                },
            },
        },
        targets = { { format = "rgba8unorm" } },
    })
    -- The shaders are compiled into the pipeline; the handles are no longer needed.
    gfx.destroy(vs)
    gfx.destroy(fs)

    -- Clip space is Vulkan's: y = -1 is the top row.
    local corners = { { -0.5, -0.5 }, { 0.5, -0.5 }, { 0.0, 0.5 } }
    local data = buffer.create(3 * 24)
    for i, corner in corners do
        local at = (i - 1) * 24
        buffer.writef32(data, at, corner[1])
        buffer.writef32(data, at + 4, corner[2])
        buffer.writef32(data, at + 8, 1.0)
        buffer.writef32(data, at + 12, 0.5)
        buffer.writef32(data, at + 16, 0.25)
        buffer.writef32(data, at + 20, 1.0)
    end
    vertices = gfx.create_buffer({ size = 3 * 24, usage = { "vertex" } })
    gfx.write_buffer(vertices, 0, data)

    local index_data = buffer.create(6)
    buffer.writeu16(index_data, 0, 0)
    buffer.writeu16(index_data, 2, 1)
    buffer.writeu16(index_data, 4, 2)
    indices = gfx.create_buffer({ size = 6, usage = { "index" } })
    gfx.write_buffer(indices, 0, index_data)
end

function frame(target)
    gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 0, 0, 0.5, 1 } } } })
    gfx.set_pipeline(pipeline)
    gfx.set_vertex_buffer(0, vertices)
    gfx.set_index_buffer(indices, "u16")
    gfx.draw_indexed(3)
    gfx.end_pass()
end
