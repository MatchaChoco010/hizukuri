-- Example: a 3D scene. A textured cube above a textured floor, drawn in
-- perspective with a depth attachment; the texture has a mip chain and is
-- read with a sampler that filters between its levels.
--
-- The shaders are Slang (`shaders/cube.slang`); the host loads their SPIR-V.
-- The matrix code below is the minimum the example needs.

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

-- ----- GPU objects

local TEXTURE_SIZE = 64

local pipeline, vertices, indices, scene_buffer, scene_slot, texture_slot, sampler
local scene_data = buffer.create(80)
local push_data = buffer.create(80)
local depth, depth_width, depth_height

function init()
    local vs = gfx.create_shader({ stage = "vertex", shader = "shaders/cube.slang", entry = "vs" })
    local fs = gfx.create_shader({ stage = "fragment", shader = "shaders/cube.slang", entry = "fs" })
    pipeline = gfx.create_render_pipeline({
        vertex = vs,
        fragment = fs,
        vertex_buffers = {
            {
                stride = STRIDE,
                attributes = {
                    { location = 0, format = "f32x3", offset = 0 },
                    { location = 1, format = "f32x3", offset = 12 },
                    { location = 2, format = "f32x2", offset = 24 },
                },
            },
        },
        targets = { { format = "rgba8unorm" } },
        depth = { format = "d32float", test = true, write = true, compare = "less" },
        -- The triangles of the meshes are counter-clockwise seen from
        -- outside, and the projection shows them upright, so they are
        -- counter-clockwise on the screen too.
        cull = "back",
        front_face = "ccw",
    })
    gfx.destroy(vs)
    gfx.destroy(fs)

    -- Six faces of the cube, then the floor (one large quad, its texture
    -- repeated 8 times).
    local vertex_data = buffer.create(7 * 4 * STRIDE)
    local index_data = buffer.create(7 * 6 * 2)
    local quads = 0
    for _, face in FACES do
        quads = write_quad(vertex_data, index_data, quads, face.n * 0.5, face, 0.5, 1)
    end
    write_quad(vertex_data, index_data, quads, vector.create(0, 0, 0), FACES[3], 4, 8)
    vertices = gfx.create_buffer({ size = buffer.len(vertex_data), usage = { "vertex" } })
    gfx.write_buffer(vertices, 0, vertex_data)
    indices = gfx.create_buffer({ size = buffer.len(index_data), usage = { "index" } })
    gfx.write_buffer(indices, 0, index_data)

    -- A checkerboard with a coloured border, and its mip chain.
    local texels = buffer.create(TEXTURE_SIZE * TEXTURE_SIZE * 4)
    for y = 0, TEXTURE_SIZE - 1 do
        for x = 0, TEXTURE_SIZE - 1 do
            local at = (y * TEXTURE_SIZE + x) * 4
            local edge = x < 3 or y < 3 or x >= TEXTURE_SIZE - 3 or y >= TEXTURE_SIZE - 3
            local light = (bit32.band(x, 8) == 0) == (bit32.band(y, 8) == 0)
            local r, g, b = 60, 60, 70
            if edge then
                r, g, b = 255, 200, 40
            elseif light then
                r, g, b = 235, 235, 235
            end
            buffer.writeu8(texels, at, r)
            buffer.writeu8(texels, at + 1, g)
            buffer.writeu8(texels, at + 2, b)
            buffer.writeu8(texels, at + 3, 255)
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
    sampler = gfx.create_sampler({
        mag = "linear",
        min = "linear",
        mip = "linear",
        address_u = "repeat",
        address_v = "repeat",
        anisotropy = 8,
    })

    scene_buffer = gfx.create_buffer({ size = 256, usage = { "uniform" } })
    scene_slot = gfx.create_uniform_slot(scene_buffer, 0, 80)
end

-- The depth image follows the size of the target.
local function depth_for(width, height)
    if depth and (depth_width ~= width or depth_height ~= height) then
        gfx.destroy(depth)
        depth = nil
    end
    if not depth then
        depth = gfx.create_image({ format = "d32float", width = width, height = height, usage = { "depth_target" } })
        depth_width, depth_height = width, height
    end
    return depth
end

local function draw_mesh(model, tint, first_index, index_count)
    write_mat4(push_data, 0, model)
    buffer.writef32(push_data, 64, tint[1])
    buffer.writef32(push_data, 68, tint[2])
    buffer.writef32(push_data, 72, tint[3])
    buffer.writef32(push_data, 76, 1)
    gfx.push(0, push_data)
    gfx.draw_indexed(index_count, 1, first_index)
end

function frame(target, time)
    local width, height = gfx.image_info(target)
    time = time or 0

    local eye = vector.create(2.2, 2.0, 3.2)
    local view = look_at(eye, vector.create(0, 0.5, 0), vector.create(0, 1, 0))
    local projection = perspective(math.rad(50), width / height, 0.1, 50)
    write_mat4(scene_data, 0, mat4_mul(projection, view))
    local light = vector.normalize(vector.create(-0.4, -1.0, -0.6))
    buffer.writef32(scene_data, 64, light.x)
    buffer.writef32(scene_data, 68, light.y)
    buffer.writef32(scene_data, 72, light.z)
    buffer.writef32(scene_data, 76, 0)
    gfx.write_buffer(scene_buffer, 0, scene_data)

    gfx.begin_pass({
        color = { { image = target, load = "clear", clear = { 0.05, 0.07, 0.12, 1 } } },
        depth = { image = depth_for(width, height), load = "clear", clear = 1.0, store = false },
    })
    gfx.set_pipeline(pipeline)
    gfx.set_vertex_buffer(0, vertices)
    gfx.set_index_buffer(indices, "u16")
    gfx.bind(0, scene_slot)
    gfx.bind(1, texture_slot)
    gfx.bind(2, sampler)
    -- The floor is drawn after the cube: the depth test keeps the cube in
    -- front of it.
    local cube = mat4_mul(translation(0, 0.75, 0), mat4_mul(rotation_y(0.6 + time), rotation_x(0.35)))
    draw_mesh(cube, { 1.0, 0.55, 0.35 }, 0, 36)
    draw_mesh(translation(0, 0, 0), { 1, 1, 1 }, 36, 6)
    gfx.end_pass()
end
