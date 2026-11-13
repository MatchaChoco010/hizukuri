-- The renderer of the built-in project.
--
-- Two passes per output:
--
--   1. Every surface is drawn as a textured quad into a linear buffer, read
--      as linear light in the working space with alpha premultiplied.
--   2. That buffer is converted to the encoded values of the output.
--
-- Both ends are the `color` module's: the numbers a shader needs to read a
-- window (`color.write_window_params`, for the core's `WindowTexture` in
-- `shaders/surface.slang`) and the conversion to the output
-- (`color.present`). What lies between is this file's.
--
-- Each window is drawn with a 2D affine transform. Without one set, that is
-- the translation to the window's logical position. The same transform,
-- inverted, answers where a point of the screen lies inside a window.

local affine = hz.include("affine")
local color = hz.include("color")

local R = {}

local surface_pipeline, sampler
-- ax and ay (32 bytes), the window's params, the target size, the opacity.
local WINDOW_AT = 32
local TARGET_SIZE_AT = WINDOW_AT + color.WINDOW_PARAMS_SIZE
local OPACITY_AT = TARGET_SIZE_AT + 8
local surface_push = buffer.create(OPACITY_AT + 8)

-- window id -> transform set by `R.set_transform`
local transforms = {}
-- window id -> factor set by `R.set_brightness`
local brightness = {}

function R.init()
    surface_pipeline = gfx.create_render_pipeline({
        vertex = gfx.create_shader({ stage = "vertex", shader = "shaders/surface.slang", entry = "vs_main" }),
        fragment = gfx.create_shader({ stage = "fragment", shader = "shaders/surface.slang", entry = "fs_main" }),
        targets = { { format = color.format(), blend = "premultiplied" } },
        topology = "triangle-strip",
    })
    sampler = gfx.create_sampler({ mag = "linear", min = "linear" })
end

-- Sets the transform a window is drawn with (window-local logical
-- coordinates to global logical coordinates), or removes it with nil.
function R.set_transform(window, transform)
    transforms[window] = transform
    hz.request_frame()
end

-- Sets the working primaries of the session (`hz.set_working_space`).
function R.set_working(primaries)
    hz.set_working_space(primaries)
end

-- Sets the factor on the light of a window (1 or nil: its reference white
-- lands on the reference white of the output).
function R.set_brightness(window, factor)
    brightness[window] = factor
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

-- Draws one textured quad. `to_pixels` maps the quad's own coordinates
-- (0..width, 0..height in logical units) to pixels of the target.
local function draw_quad(to_pixels, width, height, surface, opacity, options)
    local push = surface_push
    buffer.writef32(push, 0, to_pixels[1])
    buffer.writef32(push, 4, to_pixels[2])
    buffer.writef32(push, 8, to_pixels[5])
    buffer.writef32(push, 12, width)
    buffer.writef32(push, 16, to_pixels[3])
    buffer.writef32(push, 20, to_pixels[4])
    buffer.writef32(push, 24, to_pixels[6])
    buffer.writef32(push, 28, height)
    color.write_window_params(push, WINDOW_AT, surface, options)
    buffer.writef32(push, OPACITY_AT, opacity)
    gfx.push(0, push)
    gfx.bind(0, surface.texture)
    gfx.draw(4)
end

-- `items`, when given, is what to draw instead of the windows, bottom to
-- top: { id = window id, transform = { a, b, c, d, tx, ty }, opacity = 0..1,
-- brightness = factor on the window's light (1 when left out) }.
-- `id` may name a closed window that is kept (`hz.keep_closed`). Without
-- it, every window is drawn with `R.transform_of`, its opacity and the
-- brightness set by `R.set_brightness`.
function R.frame(output, target, items)
    local composite = color.composite_for(output)

    -- Global logical coordinates to pixels of this output.
    local to_output = affine.multiply(affine.scale(output.scale), affine.translate(-output.x, -output.y))
    local r, g, b = hz.background()
    local br, bg, bb, ba = hz.color(r, g, b)
    local cursor = hz.cursor()

    gfx.begin_pass({ color = { { image = composite.image, load = "clear", clear = { br, bg, bb, ba } } } })
    gfx.set_pipeline(surface_pipeline)
    gfx.bind(1, sampler)
    buffer.writef32(surface_push, TARGET_SIZE_AT, output.width)
    buffer.writef32(surface_push, TARGET_SIZE_AT + 4, output.height)

    if not items then
        items = {}
        for _, window in hz.windows() do
            table.insert(items, {
                id = window.id,
                transform = R.transform_of(window),
                opacity = window.opacity,
                brightness = brightness[window.id],
            })
        end
    end
    local options = {}
    for _, item in items do
        local to_pixels = affine.multiply(to_output, item.transform)
        options.brightness = item.brightness
        for _, surface in hz.surfaces(item.id) do
            if surface.texture and surface.uv then
                draw_quad(
                    affine.multiply(to_pixels, affine.translate(surface.x, surface.y)),
                    surface.width,
                    surface.height,
                    surface,
                    item.opacity,
                    options
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

    color.present(composite, output, target)
end

-- The topmost surface at a point of the screen (global logical
-- coordinates), with the point in that surface's coordinates. `accepts`
-- tells whether a surface takes input at a point of its own.
-- `transform_of(window)`, when given, is where each window is drawn instead
-- of `R.transform_of`.
function R.hit_test(x, y, accepts, transform_of)
    transform_of = transform_of or R.transform_of
    local windows = hz.windows()
    for i = #windows, 1, -1 do
        local window = windows[i]
        local inverse = affine.invert(transform_of(window))
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
function R.locate(surface_id, x, y, transform_of)
    transform_of = transform_of or R.transform_of
    for _, window in hz.windows() do
        for _, surface in hz.surfaces(window.id) do
            if surface.id == surface_id then
                local inverse = affine.invert(transform_of(window))
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
