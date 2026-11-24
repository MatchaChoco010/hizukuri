-- The built-in session without its window manager: the built-in renderer
-- draws, and the project defines none of the window management callbacks
-- (`window_opened`, `key`, ...). The core's fallback places the windows and
-- gives the focus.
--
-- Tests include it as `session`, in place of the built-in one, so that a
-- project written against the built-in session runs without the tiling.

local render = hz.include("render")

function init(saved)
    render.init()
    if saved then
        render.restore(saved.transforms)
    end
end

function unload()
    return {
        transforms = render.save(),
    }
end

function frame(output, target)
    render.frame(output, target)
end

function hit_test(x, y, accepts)
    return render.hit_test(x, y, accepts)
end

function locate(surface, x, y)
    return render.locate(surface, x, y)
end

-- Hooks for tools and tests, as in the built-in session.

function set_transform(window, transform)
    render.set_transform(window, transform)
end

function set_brightness(window, factor)
    render.set_brightness(window, factor)
end

function set_working_space(name)
    hz.set_working_space(name)
end

function working_space()
    return (hz.working_space())
end
