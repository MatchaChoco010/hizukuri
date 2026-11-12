-- Benchmark script. Written in the subset shared by Luau and Lua 5.5, so the
-- same file runs on both VMs. `buffer` (Luau) and `string.pack` (Lua) variants
-- are chosen at load time.

local QUAD = [[
struct Push {
    rect: vec4<f32>,
    target_size: vec2<f32>,
    rotation: vec2<f32>,
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

local ENCODE = [[
@group(0) @binding(0) var linear_target: texture_2d<f32>;

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

@fragment
fn fs(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let linear = textureLoad(linear_target, vec2<i32>(position.xy), 0);
    return vec4(srgb_oetf(clamp(linear.rgb, vec3(0.0), vec3(1.0))), linear.a);
}
]]

local pipelines = {}
local encode, sampler
local slots = {}
local linear_image, linear_slot

local function quad_pipeline(format, blend)
    return gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", source = QUAD, entry = "vs" }),
        fragment = gfx.create_shader({ stage = "fragment", source = QUAD, entry = "fs" }),
        targets = { { format = format, blend = blend } },
    })
end

function init(textures)
    pipelines.rgba8 = quad_pipeline("rgba8unorm", nil)
    sampler = gfx.create_sampler({ mag = "nearest", min = "nearest" })
    for i = 1, #textures do
        slots[i] = gfx.create_texture_slot(textures[i])
    end
end

-- One "draw unit" is what a compositor does per window: set the per-window
-- push data, point the texture binding at the window's slot, draw a quad.

local function begin(target)
    gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 0, 0, 0, 1 } } } })
    gfx.set_pipeline(pipelines.rgba8)
    gfx.bind(1, sampler)
    gfx.push_f32(0, 0, 0, 4, 4, 256, 256, 1, 0, 1, 1, 1, 1)
end

-- Twelve numbers per unit through a variadic call.
function frame_f32x12(target, n)
    begin(target)
    local count = #slots
    for i = 1, n do
        local x = (i % 64) * 4
        local y = (i // 64 % 64) * 4
        gfx.push_f32(0, x, y, 4, 4, 256, 256, 1, 0, 1, 1, 1, 1)
        gfx.bind(0, slots[i % count + 1])
        gfx.draw(6)
    end
    gfx.end_pass()
end

-- Four numbers per unit (only the rectangle changes).
function frame_f32x4(target, n)
    begin(target)
    local count = #slots
    for i = 1, n do
        local x = (i % 64) * 4
        local y = (i // 64 % 64) * 4
        gfx.push_f32(0, x, y, 4, 4)
        gfx.bind(0, slots[i % count + 1])
        gfx.draw(6)
    end
    gfx.end_pass()
end

-- The rectangle packed into bytes by the script, one `push` per unit.
if buffer then
    local bytes = buffer.create(16)
    function frame_bytes(target, n)
        begin(target)
        local count = #slots
        buffer.writef32(bytes, 8, 4)
        buffer.writef32(bytes, 12, 4)
        for i = 1, n do
            buffer.writef32(bytes, 0, (i % 64) * 4)
            buffer.writef32(bytes, 4, (i // 64 % 64) * 4)
            gfx.push(0, bytes)
            gfx.bind(0, slots[i % count + 1])
            gfx.draw(6)
        end
        gfx.end_pass()
    end
else
    local pack = string.pack
    function frame_bytes(target, n)
        begin(target)
        local count = #slots
        for i = 1, n do
            gfx.push(0, pack("<ffff", (i % 64) * 4, (i // 64 % 64) * 4, 4, 4))
            gfx.bind(0, slots[i % count + 1])
            gfx.draw(6)
        end
        gfx.end_pass()
    end
end

-- The same number of boundary crossings, with nothing behind them.
function frame_noop(target, n)
    local count = #slots
    for i = 1, n do
        local x = (i % 64) * 4
        local y = (i // 64 % 64) * 4
        gfx.noop(x, y)
        gfx.noop(0, slots[i % count + 1])
        gfx.noop(6, 0)
    end
end

-- The loop alone.
local sink = 0
function frame_empty(target, n)
    local count = #slots
    local acc = 0
    for i = 1, n do
        local x = (i % 64) * 4
        local y = (i // 64 % 64) * 4
        acc = acc + x + y + slots[i % count + 1]
    end
    sink = acc
end

-- Calls that fail and are caught by the script.
function frame_errors(target, n)
    gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 0, 0, 0, 1 } } } })
    local failed = 0
    for i = 1, n do
        if not pcall(gfx.draw, 6) then
            failed = failed + 1
        end
    end
    gfx.end_pass()
    assert(failed == n)
end

-- 4K: composite n windows into linear RGBA16F, then encode to the output.
function init_4k(width, height)
    pipelines.linear = quad_pipeline("rgba16float", "premultiplied")
    encode = gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", source = ENCODE, entry = "vs" }),
        fragment = gfx.create_shader({ stage = "fragment", source = ENCODE, entry = "fs" }),
        targets = { { format = "rgba8unorm" } },
    })
    linear_image = gfx.create_image({
        format = "rgba16float",
        width = width,
        height = height,
        usage = { "color_target", "sampled" },
    })
    linear_slot = gfx.create_texture_slot(linear_image)
end

function frame_4k(output, n, width, height, tick)
    gfx.begin_pass({ color = { { image = linear_image, load = "clear", clear = { 0.1, 0.1, 0.1, 1 } } } })
    gfx.set_pipeline(pipelines.linear)
    gfx.bind(1, sampler)
    gfx.push_f32(0, 0, 0, 4, 4, width, height, 1, 0, 1, 1, 1, 1)
    -- Tile the screen: every pixel is covered about once whatever n is.
    local columns = math.ceil(math.sqrt(n * width / height))
    local rows = math.ceil(n / columns)
    local w = width / columns
    local h = height / rows
    local count = #slots
    for i = 0, n - 1 do
        local x = (i % columns) * w + (tick % 3)
        local y = (i // columns) * h
        gfx.push_f32(0, x, y, w - 1, h - 1)
        gfx.bind(0, slots[i % count + 1])
        gfx.draw(6)
    end
    gfx.end_pass()

    gfx.begin_pass({ color = { { image = output, load = "dont-care" } } })
    gfx.set_pipeline(encode)
    gfx.bind(0, linear_slot)
    gfx.draw(3)
    gfx.end_pass()
end

-- Garbage collection: a live heap of `live` nodes, `n` short-lived tables per
-- frame. The nodes are kept in tables of `chunk` nodes each: `chunk == live`
-- is one flat array, a smaller `chunk` is a two-level structure.
local scene = {}
local scene_live = 0
local scene_chunk = 1
local frame_number = 0

local function new_node(i)
    return { x = i, y = i * 2, name = "node" .. i, children = {} }
end

function gc_setup(live, chunk)
    scene = {}
    scene_live = live
    scene_chunk = chunk
    for c = 1, math.ceil(live / chunk) do
        local nodes = {}
        for j = 1, chunk do
            nodes[j] = new_node((c - 1) * chunk + j)
        end
        scene[c] = nodes
    end
end

function gc_frame(n)
    frame_number = frame_number + 1
    local live = scene_live
    local chunk = scene_chunk
    local list = {}
    for i = 1, n do
        local k = (i * 7919 + frame_number) % live
        local node = scene[k // chunk + 1][k % chunk + 1]
        list[i] = { x = node.x + i, y = node.y, w = 4, h = 4 }
    end
    local sum = 0
    for i = 1, n do
        sum = sum + list[i].x
    end
    -- Replace a few live nodes, as a scene that changes would.
    for j = 1, 8 do
        local k = (frame_number * 31 + j * 977) % live
        scene[k // chunk + 1][k % chunk + 1] = new_node(k)
    end
    sink = sum
end

-- Handles for the host's own (script-free) version of the draw loop.
function export()
    return pipelines.rgba8, sampler, slots
end
