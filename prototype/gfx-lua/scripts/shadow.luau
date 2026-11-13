-- Example: a lit 3D scene with shadows. Three cubes on a floor, one
-- directional light.
--
-- Three passes: the shadow map (a depth image, drawn from the light and
-- read with a comparison sampler), the scene (multisampled, in linear light
-- with values above 1, resolved by the pass), and the output (tone-mapped).
-- Every object's matrix lives in one storage buffer; the cubes are one
-- instanced draw.
--
-- The shaders are Slang (`shaders/shadow.slang`); the host loads their
-- SPIR-V.

-- ----- 4x4 matrices: 16 numbers, row by row; they multiply column vectors.

-- Written out: the same product with three nested loops takes about
-- seven times as long, and a scene multiplies matrices for every object.
local function mat4_mul(a, b)
    local a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13, a14, a15, a16 = table.unpack(a, 1, 16)
    local b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15, b16 = table.unpack(b, 1, 16)
    return {
        a1 * b1 + a2 * b5 + a3 * b9 + a4 * b13,
        a1 * b2 + a2 * b6 + a3 * b10 + a4 * b14,
        a1 * b3 + a2 * b7 + a3 * b11 + a4 * b15,
        a1 * b4 + a2 * b8 + a3 * b12 + a4 * b16,
        a5 * b1 + a6 * b5 + a7 * b9 + a8 * b13,
        a5 * b2 + a6 * b6 + a7 * b10 + a8 * b14,
        a5 * b3 + a6 * b7 + a7 * b11 + a8 * b15,
        a5 * b4 + a6 * b8 + a7 * b12 + a8 * b16,
        a9 * b1 + a10 * b5 + a11 * b9 + a12 * b13,
        a9 * b2 + a10 * b6 + a11 * b10 + a12 * b14,
        a9 * b3 + a10 * b7 + a11 * b11 + a12 * b15,
        a9 * b4 + a10 * b8 + a11 * b12 + a12 * b16,
        a13 * b1 + a14 * b5 + a15 * b9 + a16 * b13,
        a13 * b2 + a14 * b6 + a15 * b10 + a16 * b14,
        a13 * b3 + a14 * b7 + a15 * b11 + a16 * b15,
        a13 * b4 + a14 * b8 + a15 * b12 + a16 * b16,
    }
end

local function translation(x, y, z)
    return { 1, 0, 0, x, 0, 1, 0, y, 0, 0, 1, z, 0, 0, 0, 1 }
end

local function rotation_y(angle)
    local c, s = math.cos(angle), math.sin(angle)
    return { c, 0, s, 0, 0, 1, 0, 0, -s, 0, c, 0, 0, 0, 0, 1 }
end

local function rotation_x(angle)
    local c, s = math.cos(angle), math.sin(angle)
    return { 1, 0, 0, 0, 0, c, -s, 0, 0, s, c, 0, 0, 0, 0, 1 }
end

-- A right-handed view space (the camera looks down -z, y is up) to Vulkan's
-- clip space: y points down, depth runs from 0 (near) to 1 (far).
local function perspective(fov_y, aspect, near, far)
    local f = 1 / math.tan(fov_y / 2)
    return {
        f / aspect, 0, 0, 0,
        0, -f, 0, 0,
        0, 0, far / (near - far), near * far / (near - far),
        0, 0, -1, 0,
    }
end

local function look_at(eye, target, up)
    local forward = vector.normalize(target - eye)
    local right = vector.normalize(vector.cross(forward, up))
    local true_up = vector.cross(right, forward)
    return {
        right.x, right.y, right.z, -vector.dot(right, eye),
        true_up.x, true_up.y, true_up.z, -vector.dot(true_up, eye),
        -forward.x, -forward.y, -forward.z, vector.dot(forward, eye),
        0, 0, 0, 1,
    }
end

-- Slang lays a matrix out in a buffer column by column (its default;
-- `-matrix-layout-row-major` is not given when the shaders are built).
local function write_mat4(data, offset, m)
    for c = 0, 3 do
        for r = 0, 3 do
            buffer.writef32(data, offset + (c * 4 + r) * 4, m[r * 4 + c + 1])
        end
    end
end

-- ----- the meshes: position, normal, uv (32 bytes a vertex)

local STRIDE = 32

-- One quad per entry: the normal, and two axes that span the face, so that
-- (u, v, normal) is right-handed and the triangles are counter-clockwise
-- seen from outside.
local FACES = {
    { n = vector.create(1, 0, 0), u = vector.create(0, 0, -1), v = vector.create(0, 1, 0) },
    { n = vector.create(-1, 0, 0), u = vector.create(0, 0, 1), v = vector.create(0, 1, 0) },
    { n = vector.create(0, 1, 0), u = vector.create(1, 0, 0), v = vector.create(0, 0, -1) },
    { n = vector.create(0, -1, 0), u = vector.create(1, 0, 0), v = vector.create(0, 0, 1) },
    { n = vector.create(0, 0, 1), u = vector.create(1, 0, 0), v = vector.create(0, 1, 0) },
    { n = vector.create(0, 0, -1), u = vector.create(-1, 0, 0), v = vector.create(0, 1, 0) },
}

local CORNERS = { { -1, -1 }, { 1, -1 }, { 1, 1 }, { -1, 1 } }

-- Appends a quad centred at `center` to the vertex and index data and
-- returns the number of quads written so far.
local function write_quad(vertices, indices, quad, center, face, half, uv_scale)
    for k, corner in CORNERS do
        local p = center + face.u * (corner[1] * half) + face.v * (corner[2] * half)
        local at = (quad * 4 + k - 1) * STRIDE
        buffer.writef32(vertices, at, p.x)
        buffer.writef32(vertices, at + 4, p.y)
        buffer.writef32(vertices, at + 8, p.z)
        buffer.writef32(vertices, at + 12, face.n.x)
        buffer.writef32(vertices, at + 16, face.n.y)
        buffer.writef32(vertices, at + 20, face.n.z)
        buffer.writef32(vertices, at + 24, (corner[1] * 0.5 + 0.5) * uv_scale)
        buffer.writef32(vertices, at + 28, (0.5 - corner[2] * 0.5) * uv_scale)
    end
    for k, i in { 0, 1, 2, 0, 2, 3 } do
        buffer.writeu16(indices, (quad * 6 + k - 1) * 2, quad * 4 + i)
    end
    return quad + 1
end

-- The light's view volume: a box, mapped to Vulkan's clip space the same
-- way (y down, depth from 0 at `near` to 1 at `far`).
local function orthographic(half_width, half_height, near, far)
    return {
        1 / half_width, 0, 0, 0,
        0, -1 / half_height, 0, 0,
        0, 0, 1 / (near - far), near / (near - far),
        0, 0, 0, 1,
    }
end

local function scaling(s)
    return { s, 0, 0, 0, 0, s, 0, 0, 0, 0, s, 0, 0, 0, 0, 1 }
end

-- ----- GPU objects

local TEXTURE_SIZE = 64
local SHADOW_SIZE = 1024
local SAMPLES = 4
local HDR = "rgba16float"

local shadow_pipeline, scene_pipeline, present_pipeline
local vertices, indices, scene_buffer, scene_slot, instance_buffer, instance_slot
local texture_slot, albedo_sampler, shadow_map, shadow_slot, shadow_sampler
local scene_data = buffer.create(160)
local targets

-- The objects: a model matrix and a tint each. The cubes come first; the
-- floor is the last instance.
local OBJECTS = {
    { model = translation(0, 0.5, 0), tint = { 1.0, 0.45, 0.25 } },
    { model = mat4_mul(translation(-1.8, 0.3, -0.6), mat4_mul(rotation_y(0.5), scaling(0.6))), tint = { 0.3, 0.6, 1.0 } },
    { model = mat4_mul(translation(1.4, 0.25, -1.2), mat4_mul(rotation_y(-0.3), scaling(0.5))), tint = { 0.5, 1.0, 0.4 } },
    { model = translation(0, 0, 0), tint = { 1, 1, 1 } },
}
local CUBES = #OBJECTS - 1

local LIGHT = vector.normalize(vector.create(-0.6, -1.0, 0.5))
local EYE = vector.create(3.2, 2.6, 4.2)
local view_proj

local VERTEX_BUFFERS = {
    {
        stride = STRIDE,
        attributes = {
            { location = 0, format = "f32x3", offset = 0 },
            { location = 1, format = "f32x3", offset = 12 },
            { location = 2, format = "f32x2", offset = 24 },
        },
    },
}

function init()
    local function shader(stage, entry)
        return gfx.create_shader({ stage = stage, shader = "shaders/shadow.slang", entry = entry })
    end
    -- The shadow pass has no colour target. The bias moves the stored depth
    -- away from the light, so that a lit surface does not shadow itself.
    shadow_pipeline = gfx.create_render_pipeline({
        vertex = shader("vertex", "vs_shadow"),
        fragment = shader("fragment", "fs_shadow"),
        vertex_buffers = VERTEX_BUFFERS,
        depth = { format = "d32float", compare = "less", bias = { constant = 2, slope = 2.5 } },
        cull = "back",
    })
    scene_pipeline = gfx.create_render_pipeline({
        vertex = shader("vertex", "vs"),
        fragment = shader("fragment", "fs"),
        vertex_buffers = VERTEX_BUFFERS,
        targets = { { format = HDR } },
        depth = { format = "d32float" },
        cull = "back",
        samples = SAMPLES,
    })
    present_pipeline = gfx.create_render_pipeline({
        vertex = shader("vertex", "vs_present"),
        fragment = shader("fragment", "fs_present"),
        targets = { { format = "rgba8unorm" } },
    })

    -- A unit cube (six quads), then the floor (one quad, its texture
    -- repeated).
    local vertex_data = buffer.create(7 * 4 * STRIDE)
    local index_data = buffer.create(7 * 6 * 2)
    local quads = 0
    for _, face in FACES do
        quads = write_quad(vertex_data, index_data, quads, face.n * 0.5, face, 0.5, 1)
    end
    write_quad(vertex_data, index_data, quads, vector.create(0, 0, 0), FACES[3], 5, 10)
    vertices = gfx.create_buffer({ size = buffer.len(vertex_data), usage = { "vertex" } })
    gfx.write_buffer(vertices, 0, vertex_data)
    indices = gfx.create_buffer({ size = buffer.len(index_data), usage = { "index" } })
    gfx.write_buffer(indices, 0, index_data)

    -- Every object's matrix and tint in one storage buffer; the shaders
    -- index it with the instance.
    local instance_data = buffer.create(#OBJECTS * 80)
    for i, object in OBJECTS do
        local at = (i - 1) * 80
        write_mat4(instance_data, at, object.model)
        buffer.writef32(instance_data, at + 64, object.tint[1])
        buffer.writef32(instance_data, at + 68, object.tint[2])
        buffer.writef32(instance_data, at + 72, object.tint[3])
        buffer.writef32(instance_data, at + 76, 1)
    end
    instance_buffer = gfx.create_buffer({ size = 512, usage = { "storage" } })
    gfx.write_buffer(instance_buffer, 0, instance_data)
    instance_slot = gfx.create_storage_buffer_slot(instance_buffer, 0, #OBJECTS * 80)

    local texels = buffer.create(TEXTURE_SIZE * TEXTURE_SIZE * 4)
    for y = 0, TEXTURE_SIZE - 1 do
        for x = 0, TEXTURE_SIZE - 1 do
            local light = (bit32.band(x, 16) == 0) == (bit32.band(y, 16) == 0)
            local v = if light then 235 else 170
            buffer.writeu32(texels, (y * TEXTURE_SIZE + x) * 4, 0xff000000 + v * 0x010101)
        end
    end
    local texture = gfx.create_image({
        format = "rgba8unorm",
        width = TEXTURE_SIZE,
        height = TEXTURE_SIZE,
        mip_levels = 7,
        usage = { "sampled", "copy_src", "copy_dst" },
    })
    gfx.upload_image(texture, texels)
    gfx.generate_mipmaps(texture)
    texture_slot = gfx.create_texture_slot(texture)
    albedo_sampler = gfx.create_sampler({ mip = "linear", address_u = "repeat", address_v = "repeat", anisotropy = 8 })

    -- The shadow map is a depth image that is also sampled; its sampler
    -- compares: a read returns how much of the 2x2 texels around the
    -- coordinate lies behind the reference depth.
    shadow_map = gfx.create_image({
        format = "d32float",
        width = SHADOW_SIZE,
        height = SHADOW_SIZE,
        usage = { "depth_target", "sampled" },
    })
    shadow_slot = gfx.create_texture_slot(shadow_map)
    shadow_sampler = gfx.create_sampler({ mag = "linear", min = "linear", compare = "less-equal" })

    scene_buffer = gfx.create_buffer({ size = 256, usage = { "uniform" } })
    scene_slot = gfx.create_uniform_slot(scene_buffer, 0, 160)
end

-- The multisampled colour and depth images, and the image they resolve
-- into, follow the size of the output.
local function targets_for(width, height)
    if targets and (targets.width ~= width or targets.height ~= height) then
        gfx.destroy(targets.resolved_slot)
        gfx.destroy(targets.color)
        gfx.destroy(targets.depth)
        gfx.destroy(targets.resolved)
        targets = nil
    end
    if not targets then
        targets = { width = width, height = height }
        -- A multisampled image is only ever an attachment.
        targets.color = gfx.create_image({ format = HDR, width = width, height = height, samples = SAMPLES, usage = { "color_target" } })
        targets.depth = gfx.create_image({ format = "d32float", width = width, height = height, samples = SAMPLES, usage = { "depth_target" } })
        targets.resolved = gfx.create_image({ format = HDR, width = width, height = height, usage = { "color_target", "sampled" } })
        targets.resolved_slot = gfx.create_texture_slot(targets.resolved)
    end
    return targets
end

local function draw_objects()
    gfx.set_vertex_buffer(0, vertices)
    gfx.set_index_buffer(indices, "u16")
    gfx.bind(0, scene_slot)
    gfx.bind(1, instance_slot)
    -- The cubes: 36 indices, instances 0 to CUBES - 1. The floor: 6
    -- indices from index 36 on, the last instance.
    gfx.draw_indexed(36, CUBES)
    gfx.draw_indexed(6, 1, 36, 0, CUBES)
end

function frame(target)
    local width, height = gfx.image_info(target)
    local t = targets_for(width, height)

    local view = look_at(EYE, vector.create(0, 0.4, 0), vector.create(0, 1, 0))
    view_proj = mat4_mul(perspective(math.rad(45), width / height, 0.1, 50), view)
    local light_view = look_at(LIGHT * -12, vector.create(0, 0, 0), vector.create(0, 1, 0))
    write_mat4(scene_data, 0, view_proj)
    write_mat4(scene_data, 64, mat4_mul(orthographic(5.5, 5.5, 0.1, 30), light_view))
    buffer.writef32(scene_data, 128, LIGHT.x)
    buffer.writef32(scene_data, 132, LIGHT.y)
    buffer.writef32(scene_data, 136, LIGHT.z)
    buffer.writef32(scene_data, 144, 1 / SHADOW_SIZE)
    gfx.write_buffer(scene_buffer, 0, scene_data)

    -- 1. The shadow map: depth only.
    gfx.begin_pass({ depth = { image = shadow_map, load = "clear", clear = 1.0 } })
    gfx.set_pipeline(shadow_pipeline)
    draw_objects()
    gfx.end_pass()

    -- 2. The scene, multisampled; the pass resolves it when it ends.
    gfx.begin_pass({
        color = { { image = t.color, load = "clear", clear = { 0.02, 0.03, 0.06, 1 }, store = false, resolve = t.resolved } },
        depth = { image = t.depth, load = "clear", clear = 1.0, store = false },
    })
    gfx.set_pipeline(scene_pipeline)
    gfx.bind(2, texture_slot)
    gfx.bind(3, albedo_sampler)
    gfx.bind(4, shadow_slot)
    gfx.bind(5, shadow_sampler)
    draw_objects()
    gfx.end_pass()

    -- 3. The output: the resolved image, tone-mapped.
    gfx.begin_pass({ color = { { image = target, load = "dont-care" } } })
    gfx.set_pipeline(present_pipeline)
    gfx.bind(6, t.resolved_slot)
    gfx.draw(3)
    gfx.end_pass()
end

-- The pixel of the output that shows a point of the world, for the test.
function project(x, y, z, width, height)
    local m = view_proj
    local cx = m[1] * x + m[2] * y + m[3] * z + m[4]
    local cy = m[5] * x + m[6] * y + m[7] * z + m[8]
    local cw = m[13] * x + m[14] * y + m[15] * z + m[16]
    return (cx / cw * 0.5 + 0.5) * width, (cy / cw * 0.5 + 0.5) * height
end
