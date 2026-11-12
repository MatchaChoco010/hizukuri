-- Example 3: two passes.
--   1. Composite windows into a linear RGBA16F target (premultiplied alpha).
--   2. Encode that target with the output transfer function, in a shader.
-- Per-frame parameters travel in a uniform buffer.

local COMPOSITE = [[
struct Push {
    rect: vec4<f32>,
    target_size: vec2<f32>,
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
    let pixel = pc.rect.xy + pc.rect.zw * corner;
    var out: VsOut;
    out.position = vec4(pixel / pc.target_size * 2.0 - 1.0, 0.0, 1.0);
    out.uv = corner;
    return out;
}

@group(0) @binding(0) var window_texture: texture_2d<f32>;
@group(0) @binding(1) var window_sampler: sampler;

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(window_texture, window_sampler, in.uv);
}
]]

local ENCODE = [[
struct Globals {
    exposure: f32,
    mode: u32,           // 0: sRGB, 1: PQ (SMPTE ST 2084)
    white_nits: f32,     // luminance of linear 1.0, for PQ
}
@group(0) @binding(0) var linear_target: texture_2d<f32>;
@group(0) @binding(2) var<uniform> globals: Globals;

@vertex
fn vs(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    var corners = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(corners[index], 0.0, 1.0);
}

fn srgb_oetf(c: vec3<f32>) -> vec3<f32> {
    let low = c * 12.92;
    let high = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
    return select(high, low, c <= vec3(0.0031308));
}

fn pq_oetf(nits: vec3<f32>) -> vec3<f32> {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let y = pow(clamp(nits / 10000.0, vec3(0.0), vec3(1.0)), vec3(m1));
    return pow((c1 + c2 * y) / (1.0 + c3 * y), vec3(m2));
}

@fragment
fn fs(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let linear = textureLoad(linear_target, vec2<i32>(position.xy), 0);
    let color = linear.rgb * globals.exposure;
    if globals.mode == 0u {
        return vec4(srgb_oetf(clamp(color, vec3(0.0), vec3(1.0))), linear.a);
    }
    return vec4(pq_oetf(color * globals.white_nits), linear.a);
}
]]

local composite, sampler
local encoders = {} -- output format -> pipeline
local encode_vs, encode_fs
local globals_buffer, globals_slot
local globals_data = buffer.create(16)
local linear_image, linear_slot, linear_width, linear_height
local slots = {}

function init()
    composite = gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", source = COMPOSITE, entry = "vs" }),
        fragment = gfx.create_shader({ stage = "fragment", source = COMPOSITE, entry = "fs" }),
        targets = { { format = "rgba16float", blend = "premultiplied" } },
    })
    encode_vs = gfx.create_shader({ stage = "vertex", source = ENCODE, entry = "vs" })
    encode_fs = gfx.create_shader({ stage = "fragment", source = ENCODE, entry = "fs" })
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
