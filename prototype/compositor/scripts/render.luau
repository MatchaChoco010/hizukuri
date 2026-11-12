-- The renderer of the built-in project.
--
-- Two passes per output:
--
--   1. Every surface is drawn as a textured quad into a linear RGBA16F
--      buffer, converted from what its values mean (its image description)
--      to linear light in the working primaries. Alpha is premultiplied.
--   2. That buffer is converted to the encoded values of the output.
--
-- The working primaries are this script's choice (`R.working`). The numbers
-- of the conversions come from the core (`hz.decode_transform`,
-- `hz.encode_transform`); applying them is done here, in the shaders.
--
-- Each window is drawn with a 2D affine transform. Without one set, that is
-- the translation to the window's logical position. The same transform,
-- inverted, answers where a point of the screen lies inside a window.

local affine = hz.include("affine")
local COLOR = hz.include("color")

local R = {}

-- Primaries of the linear working space: a name the core knows ("bt2020",
-- "srgb", "display-p3", "adobe-rgb") or eight chromaticity coordinates
-- (red x, y, green x, y, blue x, y, white x, y).
R.working = "bt2020"
local working_key = "bt2020"

local SURFACE = COLOR .. [[
const ALPHA_PREMULTIPLIED_ELECTRICAL: u32 = 0u;
const ALPHA_PREMULTIPLIED_OPTICAL: u32 = 1u;

struct Push {
    ax: vec4<f32>,      // a, b, tx, quad width   (pixel = a*x + b*y + tx)
    ay: vec4<f32>,      // c, d, ty, quad height  (pixel = c*x + d*y + ty)
    uv_u: vec4<f32>,    // a, b, tx: u = a*s + b*t + tx over the unit square
    uv_v: vec4<f32>,    // c, d, ty: v = c*s + d*t + ty
    m0: vec4<f32>,      // rows of the color matrix (xyz)
    m1: vec4<f32>,
    m2: vec4<f32>,
    target_size: vec2<f32>,
    tf_param: f32,
    opacity: f32,
    tf: u32,
    alpha_mode: u32,
}
var<immediate> pc: Push;

@group(0) @binding(0) var u_texture: texture_2d<f32>;
@group(0) @binding(1) var u_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    // 4-vertex triangle strip.
    let corner = vec2<f32>(f32(index & 1u), f32((index >> 1u) & 1u));
    let local = corner * vec2<f32>(pc.ax.w, pc.ay.w);
    let pixel = vec2<f32>(
        dot(pc.ax.xy, local) + pc.ax.z,
        dot(pc.ay.xy, local) + pc.ay.z,
    );
    var out: VertexOutput;
    out.uv = vec2<f32>(
        dot(pc.uv_u.xy, corner) + pc.uv_u.z,
        dot(pc.uv_v.xy, corner) + pc.uv_v.z,
    );
    out.position = vec4<f32>(pixel / pc.target_size * 2.0 - vec2<f32>(1.0), 0.0, 1.0);
    return out;
}

fn to_working(linear: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(dot(pc.m0.xyz, linear), dot(pc.m1.xyz, linear), dot(pc.m2.xyz, linear));
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSampleLevel(u_texture, u_sampler, in.uv, 0.0);
    let alpha = texel.a;
    var premultiplied: vec3<f32>;
    if pc.alpha_mode == ALPHA_PREMULTIPLIED_ELECTRICAL {
        if alpha <= 0.0 {
            premultiplied = vec3<f32>(0.0);
        } else {
            premultiplied = to_working(tf_decode(texel.rgb / alpha, pc.tf, pc.tf_param)) * alpha;
        }
    } else if pc.alpha_mode == ALPHA_PREMULTIPLIED_OPTICAL {
        premultiplied = to_working(tf_decode(texel.rgb, pc.tf, pc.tf_param));
    } else {
        premultiplied = to_working(tf_decode(texel.rgb, pc.tf, pc.tf_param)) * alpha;
    }
    return vec4<f32>(premultiplied, alpha) * pc.opacity;
}
]]

local ENCODE = COLOR .. [[
struct Push {
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
    tf: u32,
    tf_param: f32,
    clip: u32,
}
var<immediate> pc: Push;

@group(0) @binding(0) var u_composite: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let corner = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(corner * 2.0 - vec2<f32>(1.0), 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) frag_coord: vec4<f32>) -> @location(0) vec4<f32> {
    let c = textureLoad(u_composite, vec2<i32>(frag_coord.xy), 0);
    var linear = vec3<f32>(dot(pc.m0.xyz, c.rgb), dot(pc.m1.xyz, c.rgb), dot(pc.m2.xyz, c.rgb));
    if pc.clip != 0u {
        linear = clamp(linear, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    return vec4<f32>(tf_encode(linear, pc.tf, pc.tf_param), c.a);
}
]]

local surface_pipeline, sampler
local encode_shaders
local encode_pipelines = {} -- output format -> pipeline
local composites = {} -- output id -> { image, slot, width, height }
local decode_cache = {} -- working .. color -> transform
local surface_push = buffer.create(144)
local encode_push = buffer.create(64)

-- window id -> transform set by `R.set_transform`
local transforms = {}

function R.init()
    surface_pipeline = gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", source = SURFACE, entry = "vs_main" }),
        fragment = gfx.create_shader({ stage = "fragment", source = SURFACE, entry = "fs_main" }),
        targets = { { format = "rgba16float", blend = "premultiplied" } },
        topology = "triangle-strip",
    })
    encode_shaders = {
        vertex = gfx.create_shader({ stage = "vertex", source = ENCODE, entry = "vs_main" }),
        fragment = gfx.create_shader({ stage = "fragment", source = ENCODE, entry = "fs_main" }),
    }
    sampler = gfx.create_sampler({ mag = "linear", min = "linear" })
end

-- Sets the working primaries. Cached conversions are dropped.
function R.set_working(primaries)
    -- Fails here, for the caller, when the core does not accept them.
    hz.decode_color(0, 0, 0, 1, primaries)
    R.working = primaries
    working_key = type(primaries) == "table" and table.concat(primaries, ",") or primaries
    decode_cache = {}
    hz.request_frame()
end

-- Sets the transform a window is drawn with (window-local logical
-- coordinates to global logical coordinates), or removes it with nil.
function R.set_transform(window, transform)
    transforms[window] = transform
    hz.request_frame()
end

-- The transform a window is drawn with.
function R.transform_of(window)
    return transforms[window.id] or affine.translate(window.x, window.y)
end

function R.save()
    return transforms
end

function R.restore(saved)
    transforms = saved or {}
end

local function encode_pipeline(format)
    local pipeline = encode_pipelines[format]
    if not pipeline then
        pipeline = gfx.create_render_pipeline({
            vertex = encode_shaders.vertex,
            fragment = encode_shaders.fragment,
            targets = { { format = format } },
        })
        encode_pipelines[format] = pipeline
    end
    return pipeline
end

local function composite_for(output)
    local composite = composites[output.id]
    if composite and (composite.width ~= output.width or composite.height ~= output.height) then
        gfx.destroy(composite.slot)
        gfx.destroy(composite.image)
        composite = nil
    end
    if not composite then
        local image = gfx.create_image({
            format = "rgba16float",
            width = output.width,
            height = output.height,
            usage = { "color_target", "sampled", "copy_src" },
        })
        composite = {
            image = image,
            slot = gfx.create_texture_slot(image),
            width = output.width,
            height = output.height,
        }
        composites[output.id] = composite
    end
    return composite
end

local function decode_for(color)
    local key = working_key .. ":" .. color
    local transform = decode_cache[key]
    if not transform then
        transform = hz.decode_transform(color, R.working)
        decode_cache[key] = transform
    end
    return transform
end

local function write_matrix(push, at, m)
    for row = 0, 2 do
        buffer.writef32(push, at + row * 16, m[row * 3 + 1])
        buffer.writef32(push, at + row * 16 + 4, m[row * 3 + 2])
        buffer.writef32(push, at + row * 16 + 8, m[row * 3 + 3])
    end
end

local WHOLE_TEXTURE = { 1, 0, 0, 1, 0, 0 }

-- Draws one textured quad. `to_pixels` maps the quad's own coordinates
-- (0..width, 0..height in logical units) to pixels of the target.
local function draw_quad(to_pixels, width, height, surface, opacity)
    local decode = decode_for(surface.color)
    local push = surface_push
    buffer.writef32(push, 0, to_pixels[1])
    buffer.writef32(push, 4, to_pixels[2])
    buffer.writef32(push, 8, to_pixels[5])
    buffer.writef32(push, 12, width)
    buffer.writef32(push, 16, to_pixels[3])
    buffer.writef32(push, 20, to_pixels[4])
    buffer.writef32(push, 24, to_pixels[6])
    buffer.writef32(push, 28, height)
    -- Where the quad lies in the texture. The core has already folded in
    -- the shown part and how the client turned the buffer.
    local uv = surface.uv or WHOLE_TEXTURE
    buffer.writef32(push, 32, uv[1])
    buffer.writef32(push, 36, uv[2])
    buffer.writef32(push, 40, uv[5])
    buffer.writef32(push, 48, uv[3])
    buffer.writef32(push, 52, uv[4])
    buffer.writef32(push, 56, uv[6])
    write_matrix(push, 64, decode.matrix)
    buffer.writef32(push, 120, decode.transfer_param)
    buffer.writef32(push, 124, opacity)
    buffer.writeu32(push, 128, decode.transfer_id)
    buffer.writeu32(push, 132, decode.alpha_mode)
    gfx.push(0, push)
    gfx.bind(0, surface.texture)
    gfx.draw(4)
end

function R.frame(output, target)
    local composite = composite_for(output)
    hz.debug_composite(composite.image, R.working)

    -- Global logical coordinates to pixels of this output.
    local to_output = affine.multiply(affine.scale(output.scale), affine.translate(-output.x, -output.y))
    local r, g, b = hz.background()
    local br, bg, bb, ba = hz.decode_color(r, g, b, 1, R.working)
    local cursor = hz.cursor()

    gfx.begin_pass({ color = { { image = composite.image, load = "clear", clear = { br, bg, bb, ba } } } })
    gfx.set_pipeline(surface_pipeline)
    gfx.bind(1, sampler)
    buffer.writef32(surface_push, 112, output.width)
    buffer.writef32(surface_push, 116, output.height)

    for _, window in hz.windows() do
        local to_pixels = affine.multiply(to_output, R.transform_of(window))
        for _, surface in hz.surfaces(window.id) do
            if surface.texture and surface.uv then
                draw_quad(
                    affine.multiply(to_pixels, affine.translate(surface.x, surface.y)),
                    surface.width,
                    surface.height,
                    surface,
                    window.opacity
                )
            end
        end
    end

    -- What follows the pointer goes on top, snapped to whole pixels: the
    -- icon of a drag, then the cursor.
    local pointer_x, pointer_y = hz.pointer()
    local function at(x, y)
        local px, py = affine.apply(to_output, pointer_x + x, pointer_y + y)
        return { output.scale, 0, 0, output.scale, math.round(px), math.round(py) }
    end
    if pointer_x then
        for _, surface in hz.drag_icon() do
            if surface.texture and surface.uv then
                draw_quad(at(surface.x, surface.y), surface.width, surface.height, surface, 1)
            end
        end
    end
    if cursor and pointer_x then
        if cursor.texture then
            draw_quad(at(0, 0), cursor.width, cursor.height, { texture = cursor.texture, color = hz.default_color() }, 1)
        else
            for _, surface in cursor.surfaces do
                if surface.texture then
                    draw_quad(at(surface.x, surface.y), surface.width, surface.height, surface, 1)
                end
            end
        end
    end
    gfx.end_pass()

    local encode = hz.encode_transform(R.working, output.id)
    write_matrix(encode_push, 0, encode.matrix)
    buffer.writeu32(encode_push, 48, encode.transfer_id)
    buffer.writef32(encode_push, 52, encode.transfer_param)
    buffer.writeu32(encode_push, 56, encode.clip and 1 or 0)
    gfx.begin_pass({ color = { { image = target, load = "dont-care" } } })
    gfx.set_pipeline(encode_pipeline(output.format))
    gfx.bind(0, composite.slot)
    gfx.push(0, encode_push)
    gfx.draw(3)
    gfx.end_pass()
end

-- The topmost surface at a point of the screen (global logical
-- coordinates), with the point in that surface's coordinates. `accepts`
-- tells whether a surface takes input at a point of its own.
function R.hit_test(x, y, accepts)
    local windows = hz.windows()
    for i = #windows, 1, -1 do
        local window = windows[i]
        local inverse = affine.invert(R.transform_of(window))
        if inverse then
            local wx, wy = affine.apply(inverse, x, y)
            local surfaces = hz.surfaces(window.id)
            for j = #surfaces, 1, -1 do
                local surface = surfaces[j]
                local sx, sy = wx - surface.x, wy - surface.y
                if accepts(surface.id, sx, sy) then
                    return surface.id, sx, sy
                end
            end
        end
    end
    return nil
end

-- A point of the screen in the coordinates of one given surface, wherever
-- the point is.
function R.locate(surface_id, x, y)
    for _, window in hz.windows() do
        for _, surface in hz.surfaces(window.id) do
            if surface.id == surface_id then
                local inverse = affine.invert(R.transform_of(window))
                if not inverse then
                    return nil
                end
                local wx, wy = affine.apply(inverse, x, y)
                return wx - surface.x, wy - surface.y
            end
        end
    end
    return nil
end

return R
