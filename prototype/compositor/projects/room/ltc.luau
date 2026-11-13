-- Area lights by linearly transformed cosines: the script side of the
-- shader module `shaders/ltc.slang`. It uses `gfx` only.
--
--   local ltc = hz.include("ltc")
--   local tables = ltc.create_tables({
--       mat = hz.load_image("assets/ltc_mat.f32", { raw = { width = 64, height = 64, channels = 4 } }).texture,
--       amp = hz.load_image("assets/ltc_amp.f32", { raw = { width = 64, height = 64, channels = 4 } }).texture,
--   })
--   ...
--   ltc.bind_tables(tables, 0, 1, 2)   -- inside a pass, after set_pipeline

local ltc = {}

-- The tables are TABLE_SIZE × TABLE_SIZE texels of four f32.
ltc.TABLE_SIZE = 64

local function table_texture(source)
    if type(source) == "number" then
        -- A texture slot the caller made.
        return source, nil
    end
    -- The bytes of the file.
    local image = gfx.create_image({
        format = "rgba32float",
        width = ltc.TABLE_SIZE,
        height = ltc.TABLE_SIZE,
        usage = { "sampled", "copy_dst" },
    })
    gfx.upload_image(image, source)
    return gfx.create_texture_slot(image), image
end

-- The two tables and their sampler. `source.mat` and `source.amp` are each a
-- texture slot (of a 64 × 64 float image holding the file's values), or the
-- bytes of the file (a string or a buffer), which are uploaded here. The
-- tables are read with linear filtering: the image's format has to allow it.
function ltc.create_tables(source)
    local tables = {}
    tables.mat, tables.mat_image = table_texture(source.mat)
    tables.amp, tables.amp_image = table_texture(source.amp)
    tables.sampler = gfx.create_sampler({ mag = "linear", min = "linear", address_u = "clamp", address_v = "clamp" })
    return tables
end

-- Binds the tables to the bindings of a shader's `Texture2D<float4>` for the
-- matrix table, the one for the amplitude table, and their `SamplerState`.
function ltc.bind_tables(tables, mat_binding, amp_binding, sampler_binding)
    gfx.bind(mat_binding, tables.mat)
    gfx.bind(amp_binding, tables.amp)
    gfx.bind(sampler_binding, tables.sampler)
end

-- ---- a light's texture -------------------------------------------------------
--
-- A light shows a texture with mip levels; the shader reads the level whose
-- blur matches how large the light is from the shaded point. `update_light`
-- makes the levels: each one is the level above, halved and blurred by a
-- Gaussian (the shader's `LTC_SIGMA_PREFILTER`). Levels made by plain
-- halving (`gfx.generate_mipmaps`, `LTC_SIGMA_MIPMAPS`) work as well and
-- cost less; with these, the specular term is nearer to a brute-force
-- integration of the picture (ltc-check measures both).

local PREFILTER = "shaders/ltc_prefilter.slang"
local prefilter_vertex
local prefilter_sampler
local prefilter_pipelines = {} -- format -> { halve, blur }
local prefilter_push = buffer.create(8)

local function prefilter_for(format)
    local pipelines = prefilter_pipelines[format]
    if not pipelines then
        if not prefilter_vertex then
            prefilter_vertex = gfx.create_shader({ stage = "vertex", shader = PREFILTER, entry = "vs_main" })
            prefilter_sampler = gfx.create_sampler({ mag = "linear", min = "linear", address_u = "clamp", address_v = "clamp" })
        end
        local function pipeline(entry)
            return gfx.create_render_pipeline({
                vertex = prefilter_vertex,
                fragment = gfx.create_shader({ stage = "fragment", shader = PREFILTER, entry = entry }),
                targets = { { format = format } },
            })
        end
        pipelines = { halve = pipeline("fs_halve"), blur = pipeline("fs_blur") }
        prefilter_pipelines[format] = pipelines
    end
    return pipelines
end

-- A light's texture of `width` × `height` texels in `format` (a float
-- format that can be drawn to and read with linear filtering; "rgba16float"
-- when left out), with every mip level. The caller writes level 0 (the
-- window's contents in the working space: `color.blit_window(surface,
-- light.image)`), then calls `update_light`. Bind `light.slot` to the
-- shader's texture.
function ltc.create_light(width, height, format)
    format = format or "rgba16float"
    local levels = math.floor(math.log(math.max(width, height), 2)) + 1
    local function image()
        return gfx.create_image({
            format = format,
            width = width,
            height = height,
            mip_levels = levels,
            usage = { "sampled", "color_target", "copy_src", "copy_dst" },
        })
    end
    local light = {
        width = width,
        height = height,
        format = format,
        levels = levels,
        image = image(),
        -- Half of each level's blur is done into this one.
        scratch = image(),
        level_slots = {},
        scratch_slots = {},
    }
    light.slot = gfx.create_texture_slot(light.image)
    for level = 0, levels - 1 do
        light.level_slots[level] = gfx.create_texture_slot(light.image, { base_mip = level, mip_count = 1 })
        light.scratch_slots[level] = gfx.create_texture_slot(light.scratch, { base_mip = level, mip_count = 1 })
    end
    return light
end

-- Makes the levels below level 0 again. Call it outside a pass, after level
-- 0 changed (only then: the levels stay valid while the window does not
-- change).
function ltc.update_light(light)
    local pipelines = prefilter_for(light.format)
    for level = 1, light.levels - 1 do
        local width = math.max(1, bit32.rshift(light.width, level))
        local height = math.max(1, bit32.rshift(light.height, level))
        buffer.writef32(prefilter_push, 0, 1 / width)
        buffer.writef32(prefilter_push, 4, 1 / height)

        gfx.begin_pass({ color = { { image = light.scratch, mip = level, load = "dont-care" } } })
        gfx.set_pipeline(pipelines.halve)
        gfx.bind(0, light.level_slots[level - 1])
        gfx.bind(1, prefilter_sampler)
        gfx.push(0, prefilter_push)
        gfx.draw(3)
        gfx.end_pass()

        gfx.begin_pass({ color = { { image = light.image, mip = level, load = "dont-care" } } })
        gfx.set_pipeline(pipelines.blur)
        gfx.bind(0, light.scratch_slots[level])
        gfx.bind(1, prefilter_sampler)
        gfx.push(0, prefilter_push)
        gfx.draw(3)
        gfx.end_pass()
    end
end

function ltc.destroy_light(light)
    gfx.destroy(light.slot)
    for level = 0, light.levels - 1 do
        gfx.destroy(light.level_slots[level])
        gfx.destroy(light.scratch_slots[level])
    end
    gfx.destroy(light.image)
    gfx.destroy(light.scratch)
end

return ltc
