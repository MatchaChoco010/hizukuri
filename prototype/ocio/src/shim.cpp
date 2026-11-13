// The C layer over OpenColorIO's C++ API that src/lib.rs calls.
//
// Every function that can fail takes `char ** err`. On failure it returns
// null (or a non-zero status) and stores a message there, allocated with
// malloc; the caller frees it with hz_ocio_string_free. No C++ exception
// leaves this file.
//
// Strings that are returned are owned by the object they were asked from and
// stay valid until that object is freed or asked again.

#include <OpenColorIO/OpenColorIO.h>

#include <cstdlib>
#include <cstring>
#include <exception>
#include <string>

namespace OCIO = OCIO_NAMESPACE;

namespace {

void set_error(char ** err, const char * message) {
    if (err == nullptr) {
        return;
    }
    const size_t n = std::strlen(message);
    char * copy = static_cast<char *>(std::malloc(n + 1));
    if (copy != nullptr) {
        std::memcpy(copy, message, n + 1);
    }
    *err = copy;
}

// Runs `body`, turning any exception into an error message.
template <typename T, typename F> T guarded(char ** err, T failed, F && body) {
    try {
        return body();
    } catch (const std::exception & e) {
        set_error(err, e.what());
    } catch (...) {
        set_error(err, "OpenColorIO threw something that is not a std::exception");
    }
    return failed;
}

const char * or_empty(const char * s) { return s != nullptr ? s : ""; }

} // namespace

extern "C" {

struct HzConfig {
    OCIO::ConstConfigRcPtr config;
};

struct HzProcessor {
    OCIO::ConstProcessorRcPtr processor;
};

struct HzCpu {
    OCIO::ConstCPUProcessorRcPtr cpu;
};

struct HzShader {
    OCIO::GpuShaderDescRcPtr desc;
    // The last name handed out by hz_ocio_shader_uniform.
    std::string uniform_name;
};

struct HzTexture {
    const char * texture_name;
    const char * sampler_name;
    unsigned width;
    unsigned height;
    unsigned depth;
    // 1 or 3.
    unsigned channels;
    // 2 or 3.
    unsigned dimensions;
    // 0: nearest, 1: linear.
    unsigned linear;
    // width * height * depth * channels floats.
    const float * values;
};

struct HzUniform {
    const char * name;
    // OCIO::UniformDataType.
    int type;
    size_t buffer_offset;
};

void hz_ocio_string_free(char * s) { std::free(s); }

const char * hz_ocio_version(void) { return OCIO::GetVersion(); }

// Keeps OpenColorIO from writing to the standard streams.
void hz_ocio_quiet(void) {
    try {
        OCIO::SetLoggingLevel(OCIO::LOGGING_LEVEL_NONE);
    } catch (...) {
    }
}

// --- built-in configs ---

size_t hz_ocio_builtin_count(void) {
    try {
        return OCIO::BuiltinConfigRegistry::Get().getNumBuiltinConfigs();
    } catch (...) {
        return 0;
    }
}

const char * hz_ocio_builtin_name(size_t index) {
    try {
        return or_empty(OCIO::BuiltinConfigRegistry::Get().getBuiltinConfigName(index));
    } catch (...) {
        return "";
    }
}

// --- config ---

// how: 0 from the environment (the variable OCIO), 1 from a file or an
// `ocio://` name, 2 from the name of a built-in config.
HzConfig * hz_ocio_config_open(int how, const char * spec, char ** err) {
    return guarded(err, static_cast<HzConfig *>(nullptr), [&]() {
        OCIO::ConstConfigRcPtr config;
        switch (how) {
        case 0:
            config = OCIO::Config::CreateFromEnv();
            break;
        case 1:
            config = OCIO::Config::CreateFromFile(spec);
            break;
        default:
            config = OCIO::Config::CreateFromBuiltinConfig(spec);
            break;
        }
        if (!config) {
            throw OCIO::Exception("no config was created");
        }
        return new HzConfig{config};
    });
}

void hz_ocio_config_free(HzConfig * c) { delete c; }

const char * hz_ocio_config_cache_id(const HzConfig * c, char ** err) {
    return guarded(err, static_cast<const char *>(nullptr),
                   [&]() { return or_empty(c->config->getCacheID()); });
}

const char * hz_ocio_config_name(const HzConfig * c) {
    try {
        return or_empty(c->config->getName());
    } catch (...) {
        return "";
    }
}

// what: 0 colour spaces, 1 displays, 2 roles, 3 views of the display `arg`.
int hz_ocio_config_count(const HzConfig * c, int what, const char * arg) {
    try {
        switch (what) {
        case 0:
            return c->config->getNumColorSpaces();
        case 1:
            return c->config->getNumDisplays();
        case 2:
            return c->config->getNumRoles();
        case 3:
            return c->config->getNumViews(arg);
        default:
            return 0;
        }
    } catch (...) {
        return 0;
    }
}

const char * hz_ocio_config_item(const HzConfig * c, int what, const char * arg, int index) {
    try {
        switch (what) {
        case 0:
            return or_empty(c->config->getColorSpaceNameByIndex(index));
        case 1:
            return or_empty(c->config->getDisplay(index));
        case 2:
            return or_empty(c->config->getRoleName(index));
        case 3:
            return or_empty(c->config->getView(arg, index));
        default:
            return "";
        }
    } catch (...) {
        return "";
    }
}

// The colour space a role names, or "" when the config has no such role.
const char * hz_ocio_config_role(const HzConfig * c, const char * role) {
    try {
        return or_empty(c->config->getRoleColorSpace(role));
    } catch (...) {
        return "";
    }
}

// The name of the colour space that `name` (a colour space, an alias or a
// role) resolves to, or "" when there is none.
const char * hz_ocio_config_resolve(const HzConfig * c, const char * name) {
    try {
        OCIO::ConstColorSpaceRcPtr cs = c->config->getColorSpace(name);
        return cs ? or_empty(cs->getName()) : "";
    } catch (...) {
        return "";
    }
}

// 0: scene-referred, 1: display-referred, -1: no such colour space.
int hz_ocio_config_reference_type(const HzConfig * c, const char * name) {
    try {
        OCIO::ConstColorSpaceRcPtr cs = c->config->getColorSpace(name);
        if (!cs) {
            return -1;
        }
        return cs->getReferenceSpaceType() == OCIO::REFERENCE_SPACE_DISPLAY ? 1 : 0;
    } catch (...) {
        return -1;
    }
}

const char * hz_ocio_config_default_display(const HzConfig * c) {
    try {
        return or_empty(c->config->getDefaultDisplay());
    } catch (...) {
        return "";
    }
}

const char * hz_ocio_config_default_view(const HzConfig * c, const char * display) {
    try {
        return or_empty(c->config->getDefaultView(display));
    } catch (...) {
        return "";
    }
}

// The colour space of a view (what the signal is after the view), or "".
const char * hz_ocio_config_view_colorspace(const HzConfig * c, const char * display,
                                            const char * view) {
    try {
        return or_empty(c->config->getDisplayViewColorSpaceName(display, view));
    } catch (...) {
        return "";
    }
}

// --- processors ---

HzProcessor * hz_ocio_processor_colorspaces(const HzConfig * c, const char * src,
                                            const char * dst, char ** err) {
    return guarded(err, static_cast<HzProcessor *>(nullptr), [&]() {
        return new HzProcessor{c->config->getProcessor(src, dst)};
    });
}

// From a colour space to a display and view (or back, when `inverse`).
//
// `scale` multiplies the RGB of the source before anything else (ignored
// when it is 1). `dynamic` adds controls that can be changed without a new
// processor: bit 0 an exposure (in stops, applied to the scaled source),
// bit 1 a gamma (applied to the result of the view).
HzProcessor * hz_ocio_processor_display(const HzConfig * c, const char * src,
                                        const char * display, const char * view, int inverse,
                                        double scale, unsigned dynamic, char ** err) {
    return guarded(err, static_cast<HzProcessor *>(nullptr), [&]() {
        OCIO::GroupTransformRcPtr group = OCIO::GroupTransform::Create();
        if (scale != 1.0) {
            const double m[16] = {scale, 0, 0, 0, 0, scale, 0, 0, 0, 0, scale, 0, 0, 0, 0, 1};
            OCIO::MatrixTransformRcPtr matrix = OCIO::MatrixTransform::Create();
            matrix->setMatrix(m);
            group->appendTransform(matrix);
        }
        if ((dynamic & 1u) != 0) {
            OCIO::ExposureContrastTransformRcPtr ec = OCIO::ExposureContrastTransform::Create();
            ec->setStyle(OCIO::EXPOSURE_CONTRAST_LINEAR);
            ec->makeExposureDynamic();
            group->appendTransform(ec);
        }
        OCIO::DisplayViewTransformRcPtr dv = OCIO::DisplayViewTransform::Create();
        dv->setSrc(src);
        dv->setDisplay(display);
        dv->setView(view);
        group->appendTransform(dv);
        if ((dynamic & 2u) != 0) {
            OCIO::ExposureContrastTransformRcPtr ec = OCIO::ExposureContrastTransform::Create();
            ec->setStyle(OCIO::EXPOSURE_CONTRAST_LINEAR);
            ec->setPivot(1.0);
            ec->makeGammaDynamic();
            group->appendTransform(ec);
        }
        return new HzProcessor{c->config->getProcessor(
            group, inverse != 0 ? OCIO::TRANSFORM_DIR_INVERSE : OCIO::TRANSFORM_DIR_FORWARD)};
    });
}

void hz_ocio_processor_free(HzProcessor * p) { delete p; }

const char * hz_ocio_processor_cache_id(const HzProcessor * p, char ** err) {
    return guarded(err, static_cast<const char *>(nullptr),
                   [&]() { return or_empty(p->processor->getCacheID()); });
}

int hz_ocio_processor_is_noop(const HzProcessor * p) {
    try {
        return p->processor->isNoOp() ? 1 : 0;
    } catch (...) {
        return 0;
    }
}

// --- CPU ---

// `precise`: without the approximations of log, exp and pow that the default
// optimization uses.
HzCpu * hz_ocio_processor_cpu(const HzProcessor * p, int precise, char ** err) {
    return guarded(err, static_cast<HzCpu *>(nullptr), [&]() {
        unsigned long flags = OCIO::OPTIMIZATION_DEFAULT;
        if (precise != 0) {
            flags &= ~static_cast<unsigned long>(OCIO::OPTIMIZATION_FAST_LOG_EXP_POW);
        }
        return new HzCpu{p->processor->getOptimizedCPUProcessor(
            OCIO::BIT_DEPTH_F32, OCIO::BIT_DEPTH_F32,
            static_cast<OCIO::OptimizationFlags>(flags))};
    });
}

void hz_ocio_cpu_free(HzCpu * c) { delete c; }

// Converts `count` RGBA pixels (4 floats each) in place.
int hz_ocio_cpu_apply_rgba(const HzCpu * c, float * pixels, size_t count, char ** err) {
    return guarded(err, 1, [&]() {
        if (count == 0) {
            return 0;
        }
        OCIO::PackedImageDesc image(pixels, static_cast<long>(count), 1, 4);
        c->cpu->apply(image);
        return 0;
    });
}

// type: OCIO::DynamicPropertyType (0 exposure, 1 contrast, 2 gamma).
int hz_ocio_cpu_set_dynamic(const HzCpu * c, int type, double value, char ** err) {
    return guarded(err, 1, [&]() {
        OCIO::DynamicPropertyRcPtr property =
            c->cpu->getDynamicProperty(static_cast<OCIO::DynamicPropertyType>(type));
        OCIO::DynamicPropertyValue::AsDouble(property)->setValue(value);
        return 0;
    });
}

// --- GPU shader ---

// The shader of a processor as HLSL: a function `function_name` from a
// float4 to a float4, with its textures and uniforms declared at global
// scope under names that start with `resource_prefix`. One-dimensional
// lookup tables are laid out as 2D textures.
HzShader * hz_ocio_processor_shader(const HzProcessor * p, const char * function_name,
                                    const char * resource_prefix, char ** err) {
    return guarded(err, static_cast<HzShader *>(nullptr), [&]() {
        OCIO::GpuShaderDescRcPtr desc = OCIO::GpuShaderDesc::CreateShaderDesc();
        desc->setLanguage(OCIO::GPU_LANGUAGE_HLSL_DX11);
        desc->setFunctionName(function_name);
        desc->setResourcePrefix(resource_prefix);
        desc->setAllowTexture1D(false);
        p->processor->getDefaultGPUProcessor()->extractGpuShaderInfo(desc);
        return new HzShader{desc, std::string()};
    });
}

void hz_ocio_shader_free(HzShader * s) { delete s; }

const char * hz_ocio_shader_text(const HzShader * s) {
    try {
        return or_empty(s->desc->getShaderText());
    } catch (...) {
        return "";
    }
}

const char * hz_ocio_shader_cache_id(const HzShader * s) { return or_empty(s->desc->getCacheID()); }

// The 2D textures come first, then the 3D ones.
unsigned hz_ocio_shader_texture_count(const HzShader * s) {
    return s->desc->getNumTextures() + s->desc->getNum3DTextures();
}

int hz_ocio_shader_texture(const HzShader * s, unsigned index, HzTexture * out, char ** err) {
    return guarded(err, 1, [&]() {
        const unsigned n2 = s->desc->getNumTextures();
        OCIO::Interpolation interpolation = OCIO::INTERP_UNKNOWN;
        if (index < n2) {
            OCIO::GpuShaderDesc::TextureType channel = OCIO::GpuShaderDesc::TEXTURE_RED_CHANNEL;
            OCIO::GpuShaderDesc::TextureDimensions dimensions = OCIO::GpuShaderDesc::TEXTURE_2D;
            s->desc->getTexture(index, out->texture_name, out->sampler_name, out->width,
                                out->height, channel, dimensions, interpolation);
            if (dimensions != OCIO::GpuShaderDesc::TEXTURE_2D) {
                throw OCIO::Exception("a one-dimensional texture, which was not asked for");
            }
            out->depth = 1;
            out->channels = channel == OCIO::GpuShaderDesc::TEXTURE_RED_CHANNEL ? 1 : 3;
            out->dimensions = 2;
            s->desc->getTextureValues(index, out->values);
        } else {
            unsigned edge = 0;
            s->desc->get3DTexture(index - n2, out->texture_name, out->sampler_name, edge,
                                  interpolation);
            out->width = out->height = out->depth = edge;
            out->channels = 3;
            out->dimensions = 3;
            s->desc->get3DTextureValues(index - n2, out->values);
        }
        // Tetrahedral interpolation is written out in the shader, over
        // texels read without filtering.
        out->linear = (interpolation == OCIO::INTERP_NEAREST ||
                       interpolation == OCIO::INTERP_TETRAHEDRAL)
                          ? 0
                          : 1;
        return 0;
    });
}

unsigned hz_ocio_shader_uniform_count(const HzShader * s) { return s->desc->getNumUniforms(); }

size_t hz_ocio_shader_uniform_buffer_size(const HzShader * s) {
    return s->desc->getUniformBufferSize();
}

int hz_ocio_shader_uniform(HzShader * s, unsigned index, HzUniform * out, char ** err) {
    return guarded(err, 1, [&]() {
        OCIO::GpuShaderDesc::UniformData data;
        s->uniform_name = or_empty(s->desc->getUniform(index, data));
        out->name = s->uniform_name.c_str();
        out->type = static_cast<int>(data.m_type);
        out->buffer_offset = data.m_bufferOffset;
        return 0;
    });
}

// The current value of a uniform as floats (integers and booleans are
// converted): at most `capacity` values are written. Returns how many the
// uniform has, or -1 on failure.
int hz_ocio_shader_uniform_value(const HzShader * s, unsigned index, float * out, int capacity,
                                 char ** err) {
    return guarded(err, -1, [&]() {
        OCIO::GpuShaderDesc::UniformData data;
        s->desc->getUniform(index, data);
        auto put = [&](int i, float v) {
            if (i < capacity) {
                out[i] = v;
            }
        };
        switch (data.m_type) {
        case OCIO::UNIFORM_DOUBLE:
            put(0, static_cast<float>(data.m_getDouble()));
            return 1;
        case OCIO::UNIFORM_BOOL:
            put(0, data.m_getBool() ? 1.0f : 0.0f);
            return 1;
        case OCIO::UNIFORM_FLOAT3: {
            const OCIO::Float3 & v = data.m_getFloat3();
            for (int i = 0; i < 3; ++i) {
                put(i, v[i]);
            }
            return 3;
        }
        case OCIO::UNIFORM_VECTOR_FLOAT: {
            const int n = data.m_vectorFloat.m_getSize();
            const float * v = data.m_vectorFloat.m_getVector();
            for (int i = 0; i < n; ++i) {
                put(i, v[i]);
            }
            return n;
        }
        case OCIO::UNIFORM_VECTOR_INT: {
            const int n = data.m_vectorInt.m_getSize();
            const int * v = data.m_vectorInt.m_getVector();
            for (int i = 0; i < n; ++i) {
                put(i, static_cast<float>(v[i]));
            }
            return n;
        }
        default:
            throw OCIO::Exception("a uniform of an unknown type");
        }
    });
}

int hz_ocio_shader_has_dynamic(const HzShader * s, int type) {
    try {
        return s->desc->hasDynamicProperty(static_cast<OCIO::DynamicPropertyType>(type)) ? 1 : 0;
    } catch (...) {
        return 0;
    }
}

int hz_ocio_shader_set_dynamic(HzShader * s, int type, double value, char ** err) {
    return guarded(err, 1, [&]() {
        OCIO::DynamicPropertyRcPtr property =
            s->desc->getDynamicProperty(static_cast<OCIO::DynamicPropertyType>(type));
        OCIO::DynamicPropertyValue::AsDouble(property)->setValue(value);
        return 0;
    });
}

} // extern "C"
