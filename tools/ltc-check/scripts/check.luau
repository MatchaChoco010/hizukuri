-- The script side of ltc-check. The host loads the project's `ltc` module
-- into the global `ltc`, then calls these functions.

local SHADER = "shaders/check.slang"
local PROBE_BYTES = 160
local SCENE_BYTES = 624

local tables
local vertex
local probe_pipeline
local scene_pipelines = {} -- target format -> pipeline
local scene_fragment
local light_sampler
local lights = {} -- id -> { image, slot }
local scene_buffer, scene_slot
local targets = {} -- "format:width:height" -> image

local function target(format, width, height)
    local key = `{format}:{width}:{height}`
    local image = targets[key]
    if not image then
        image = gfx.create_image({
            format = format,
            width = width,
            height = height,
            usage = { "color_target", "copy_src", "sampled" },
        })
        targets[key] = image
    end
    return image
end

-- `mat` and `amp`: the bytes of the two table files.
function setup(mat, amp)
    tables = ltc.create_tables({ mat = mat, amp = amp })
    vertex = gfx.create_shader({ stage = "vertex", shader = SHADER, entry = "vs_main" })
    probe_pipeline = gfx.create_render_pipeline({
        vertex = vertex,
        fragment = gfx.create_shader({ stage = "fragment", shader = SHADER, entry = "fs_probe" }),
        targets = { { format = "rgba32float" } },
    })
    scene_fragment = gfx.create_shader({ stage = "fragment", shader = SHADER, entry = "fs_scene" })
    light_sampler = gfx.create_sampler({
        mag = "linear",
        min = "linear",
        mip = "linear",
        address_u = "clamp",
        address_v = "clamp",
    })
    scene_buffer = gfx.create_buffer({ size = SCENE_BYTES, usage = { "uniform" } })
    scene_slot = gfx.create_uniform_slot(scene_buffer, 0, SCENE_BYTES)
end

-- A light's texture from RGBA texels in `format` ("rgba16float" when left
-- out; the texels are that format's bytes). `levels` says how the mip levels
-- below are made: "none" (there are none), "mipmaps" (by halving) or
-- "prefilter" (by the module's `update_light`). Returns the light's id.
function make_light(width, height, texels, levels, format)
    format = format or "rgba16float"
    local entry
    if levels == "prefilter" then
        local light = ltc.create_light(width, height, format)
        gfx.upload_image(light.image, texels)
        ltc.update_light(light)
        entry = { image = light.image, slot = light.slot, light = light }
    else
        local count = 1
        if levels == "mipmaps" then
            count = math.floor(math.log(math.max(width, height), 2)) + 1
        end
        local image = gfx.create_image({
            format = format,
            width = width,
            height = height,
            mip_levels = count,
            usage = { "sampled", "color_target", "copy_src", "copy_dst" },
        })
        gfx.upload_image(image, texels)
        if levels == "mipmaps" then
            gfx.generate_mipmaps(image)
        end
        entry = { image = image, slot = gfx.create_texture_slot(image) }
    end
    local id = #lights + 1
    lights[id] = entry
    return id
end

-- Makes the levels of a light again, as a frame that changed the light's
-- contents does.
function refresh_light(id)
    local entry = lights[id]
    if entry.light then
        ltc.update_light(entry.light)
    else
        gfx.generate_mipmaps(entry.image)
    end
end

-- Shades `count` points, one per pixel of a `count` × 1 target, each with
-- its own push data (`pushes`: `count` × 160 bytes), lit by the light `light`.
-- Returns the target.
function probe(count, pushes, light)
    local image = target("rgba32float", count, 1)
    gfx.begin_pass({ color = { { image = image, load = "clear", clear = { 0, 0, 0, 0 } } } })
    gfx.set_pipeline(probe_pipeline)
    ltc.bind_tables(tables, 0, 1, 2)
    gfx.bind(3, lights[light].slot)
    gfx.bind(7, light_sampler)
    for i = 0, count - 1 do
        gfx.set_scissor(i, 0, 1, 1)
        gfx.push(0, string.sub(pushes, i * PROBE_BYTES + 1, (i + 1) * PROBE_BYTES))
        gfx.draw(3)
    end
    gfx.end_pass()
    return image
end

-- Draws the scene (`data`: the 624 bytes of the shader's `Scene`) with up to
-- four lights (`ids`). Returns the target.
function draw_scene(format, width, height, data, ids)
    local pipeline = scene_pipelines[format]
    if not pipeline then
        pipeline = gfx.create_render_pipeline({
            vertex = vertex,
            fragment = scene_fragment,
            targets = { { format = format } },
        })
        scene_pipelines[format] = pipeline
    end
    local image = target(format, width, height)
    gfx.write_buffer(scene_buffer, 0, data)
    gfx.begin_pass({ color = { { image = image, load = "dont-care" } } })
    gfx.set_pipeline(pipeline)
    ltc.bind_tables(tables, 0, 1, 2)
    for binding = 3, 6 do
        local id = ids[binding - 2] or ids[1]
        gfx.bind(binding, lights[id].slot)
    end
    gfx.bind(7, light_sampler)
    gfx.bind(8, scene_slot)
    gfx.draw(3)
    gfx.end_pass()
    return image
end
