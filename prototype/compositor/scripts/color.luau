-- Color at the two ends of a renderer.
--
-- A renderer draws in the working space: linear light, 1.0 at the reference
-- white, alpha premultiplied. This module is what stands at its two ends:
--
--   in:  a window's contents, read in a shader through the core's
--        `WindowTexture` (`import color;`). `write_window_params` fills in
--        the numbers that type needs for one surface. `blit_window` copies a
--        surface into an image of the renderer's, converted.
--   out: `present` converts a buffer in the working space to what an output
--        takes.
--
-- A project that draws with its own shaders uses it as
-- `local color = hz.include("color")`.

local C = {}

-- The primaries of the linear working space (the session's: a setting of
-- the person who uses the compositor): a name the core knows or eight
-- chromaticity coordinates (red x, y, green x, y, blue x, y, white x, y).
function C.working()
    return (hz.working_space())
end

-- Sets the working primaries. Colors and images converted before keep the
-- numbers of the old ones.
function C.set_working(primaries)
    hz.set_working_space(primaries)
end

local decode_cache = {} -- color -> transform, for one working space
local decode_generation = -1

local function decode_for(color)
    local _, generation = hz.working_space()
    if generation ~= decode_generation then
        decode_cache = {}
        decode_generation = generation
    end
    local transform = decode_cache[color]
    if not transform then
        transform = hz.decode_transform(color)
        decode_cache[color] = transform
    end
    return transform
end

-- ----- in: window contents ---------------------------------------------------

-- The size in bytes of a `WindowParams` of the core's Slang module.
C.WINDOW_PARAMS_SIZE = 96

local WHOLE_TEXTURE = { 1, 0, 0, 1, 0, 0 }

-- Writes the `WindowParams` of one surface into `target` (a buffer) at
-- the byte offset `at`. `surface` is an element of `hz.surfaces`,
-- `hz.drag_icon` or of a cursor's surfaces, or any table with `color` (and
-- `uv`, when not the whole texture).
--
-- `options.brightness` scales the light of the window (1 by default: the
-- window's reference white lands on 1.0 of the working space).
function C.write_window_params(target, at, surface, options)
    if not surface.color then
        error("write_window_params: the surface has no contents yet (its `texture` is nil)", 2)
    end
    local decode = decode_for(surface.color)
    local gain = options and options.brightness or 1
    -- Where the surface lies in the texture. The core has already folded in
    -- the shown part and how the client turned the buffer.
    local uv = surface.uv or WHOLE_TEXTURE
    buffer.writef32(target, at, uv[1])
    buffer.writef32(target, at + 4, uv[2])
    buffer.writef32(target, at + 8, uv[5])
    buffer.writef32(target, at + 12, 0)
    buffer.writef32(target, at + 16, uv[3])
    buffer.writef32(target, at + 20, uv[4])
    buffer.writef32(target, at + 24, uv[6])
    buffer.writef32(target, at + 28, 0)
    local m = decode.matrix
    -- The w of the three matrix rows.
    local ootf = decode.ootf_coefficients
    for row = 0, 2 do
        local o = at + 32 + row * 16
        buffer.writef32(target, o, m[row * 3 + 1] * gain)
        buffer.writef32(target, o + 4, m[row * 3 + 2] * gain)
        buffer.writef32(target, o + 8, m[row * 3 + 3] * gain)
        buffer.writef32(target, o + 12, ootf and ootf[row + 1] or 0)
    end
    buffer.writef32(target, at + 80, decode.transfer_param)
    buffer.writef32(target, at + 84, 0)
    buffer.writeu32(target, at + 88, decode.transfer_id)
    buffer.writeu32(target, at + 92, decode.alpha_mode)
end

-- The `WindowParams` of one surface, as a new buffer of 96 bytes.
function C.window_params(surface, options)
    local params = buffer.create(C.WINDOW_PARAMS_SIZE)
    C.write_window_params(params, 0, surface, options)
    return params
end

local blit_shaders
local blit_pipelines = {} -- target format -> pipeline
local blit_samplers = {} -- filter -> sampler
local blit_push = buffer.create(C.WINDOW_PARAMS_SIZE)

-- Copies the contents of one surface over the whole of `image`, converted
-- to premultiplied linear light in the working space. A pass of its own:
-- call it outside `gfx.begin_pass` / `gfx.end_pass`.
--
-- `options.brightness`: as for `write_window_params`.
-- `options.filter`: "linear" (the default) or "nearest".
-- `options.mip`, `options.layer`: the mip level and the layer of `image` to
-- draw to. `options.attachment`: more fields of the color attachment.
function C.blit_window(surface, image, options)
    if not surface.texture then
        return false
    end
    if not blit_shaders then
        blit_shaders = {
            vertex = gfx.create_shader({ stage = "vertex", shader = "shaders/window_blit.slang", entry = "vs_main" }),
            fragment = gfx.create_shader({
                stage = "fragment",
                shader = "shaders/window_blit.slang",
                entry = "fs_main",
            }),
        }
    end
    local _, _, format = gfx.image_info(image)
    local pipeline = blit_pipelines[format]
    if not pipeline then
        pipeline = gfx.create_render_pipeline({
            vertex = blit_shaders.vertex,
            fragment = blit_shaders.fragment,
            targets = { { format = format } },
        })
        blit_pipelines[format] = pipeline
    end
    local filter = options and options.filter or "linear"
    local sampler = blit_samplers[filter]
    if not sampler then
        sampler = gfx.create_sampler({ mag = filter, min = filter })
        blit_samplers[filter] = sampler
    end
    local attachment = { image = image, load = "dont-care" }
    if options then
        attachment.mip = options.mip
        attachment.layer = options.layer
        if options.attachment then
            for key, value in options.attachment do
                attachment[key] = value
            end
        end
    end
    C.write_window_params(blit_push, 0, surface, options)
    gfx.begin_pass({ color = { attachment } })
    gfx.set_pipeline(pipeline)
    gfx.bind(0, surface.texture)
    gfx.bind(1, sampler)
    gfx.push(0, blit_push)
    gfx.draw(3)
    gfx.end_pass()
    return true
end

-- ----- out: to an output -----------------------------------------------------

-- The format of buffers that hold colors of the working space
-- (`hz.color_format()`).
function C.format()
    return hz.color_format()
end

local composites = {} -- output id -> { image, slot, width, height, format }

-- A buffer of the size of `output` to draw the frame into, in the working
-- space: `{ image, slot, width, height, format }`. Kept from frame to frame
-- and made again when the output changes size.
function C.composite_for(output)
    local composite = composites[output.id]
    if composite and (composite.width ~= output.width or composite.height ~= output.height) then
        gfx.destroy(composite.slot)
        gfx.destroy(composite.image)
        composite = nil
    end
    if not composite then
        local format = C.format()
        local image = gfx.create_image({
            format = format,
            width = output.width,
            height = output.height,
            usage = { "color_target", "sampled", "copy_src" },
        })
        composite = {
            image = image,
            slot = gfx.create_texture_slot(image),
            width = output.width,
            height = output.height,
            format = format,
        }
        composites[output.id] = composite
    end
    return composite
end

local slots = {} -- image -> texture slot, for images given bare

-- Converts `composite` (premultiplied linear light in the working space,
-- the size of the output) to what `output` takes and writes it to `target`,
-- the image `frame` was given. A pass of its own: call it last, outside
-- `gfx.begin_pass` / `gfx.end_pass`.
--
-- `composite` is what `composite_for` returned, or `{ image = , slot = }`
-- of the project's own image (`slot` from `gfx.create_texture_slot`), or a
-- bare image handle.
--
-- The conversion is the core's (`hz.present`): through the output's OCIO
-- display and view when it has them, and by its image description
-- otherwise.
function C.present(composite, output, target)
    local image, slot
    if type(composite) == "table" then
        image, slot = composite.image, composite.slot
    else
        image = composite
    end
    if not slot then
        slot = slots[image]
        if not slot then
            slot = gfx.create_texture_slot(image)
            slots[image] = slot
        end
    end
    hz.present(image, slot, output.id, target)
end

-- Forgets the texture slot `present` made for a bare image. Call it before
-- destroying such an image.
function C.forget(image)
    local slot = slots[image]
    if slot then
        gfx.destroy(slot)
        slots[image] = nil
    end
end

return C
