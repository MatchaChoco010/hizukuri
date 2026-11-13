//! OpenColorIO (OCIO) for Rust.
//!
//! OCIO has no C API, so `src/shim.cpp` wraps the part of its C++ API that is
//! needed in `extern "C"` functions, and this file wraps those. The OCIO
//! library itself is the system's shared library.
//!
//! What is here: reading a config (a built-in one, a file, or the one the
//! environment variable `OCIO` names), listing its colour spaces, displays,
//! views and roles, making processors (colour space to colour space, colour
//! space to display and view), applying a processor on the CPU, and taking a
//! processor's GPU shader out as HLSL text with its textures and uniforms.
//!
//! No C++ exception crosses the boundary: the C++ layer catches them and
//! they arrive as [`Error`].

use std::ffi::{CStr, CString, c_char, c_int, c_uint};
use std::fmt;
use std::path::Path;
use std::ptr::NonNull;

mod ffi {
    use std::ffi::{c_char, c_int, c_uint};

    #[repr(C)]
    pub struct Config {
        _private: [u8; 0],
    }
    #[repr(C)]
    pub struct Processor {
        _private: [u8; 0],
    }
    #[repr(C)]
    pub struct Cpu {
        _private: [u8; 0],
    }
    #[repr(C)]
    pub struct Shader {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct Texture {
        pub texture_name: *const c_char,
        pub sampler_name: *const c_char,
        pub width: c_uint,
        pub height: c_uint,
        pub depth: c_uint,
        pub channels: c_uint,
        pub dimensions: c_uint,
        pub linear: c_uint,
        pub values: *const f32,
    }

    #[repr(C)]
    pub struct Uniform {
        pub name: *const c_char,
        pub ty: c_int,
        pub buffer_offset: usize,
    }

    unsafe extern "C" {
        pub fn hz_ocio_string_free(s: *mut c_char);
        pub fn hz_ocio_version() -> *const c_char;
        pub fn hz_ocio_quiet();
        pub fn hz_ocio_builtin_count() -> usize;
        pub fn hz_ocio_builtin_name(index: usize) -> *const c_char;

        pub fn hz_ocio_config_open(
            how: c_int,
            spec: *const c_char,
            err: *mut *mut c_char,
        ) -> *mut Config;
        pub fn hz_ocio_config_free(c: *mut Config);
        pub fn hz_ocio_config_cache_id(c: *const Config, err: *mut *mut c_char) -> *const c_char;
        pub fn hz_ocio_config_name(c: *const Config) -> *const c_char;
        pub fn hz_ocio_config_count(c: *const Config, what: c_int, arg: *const c_char) -> c_int;
        pub fn hz_ocio_config_item(
            c: *const Config,
            what: c_int,
            arg: *const c_char,
            index: c_int,
        ) -> *const c_char;
        pub fn hz_ocio_config_role(c: *const Config, role: *const c_char) -> *const c_char;
        pub fn hz_ocio_config_resolve(c: *const Config, name: *const c_char) -> *const c_char;
        pub fn hz_ocio_config_reference_type(c: *const Config, name: *const c_char) -> c_int;
        pub fn hz_ocio_config_default_display(c: *const Config) -> *const c_char;
        pub fn hz_ocio_config_default_view(
            c: *const Config,
            display: *const c_char,
        ) -> *const c_char;
        pub fn hz_ocio_config_view_colorspace(
            c: *const Config,
            display: *const c_char,
            view: *const c_char,
        ) -> *const c_char;

        pub fn hz_ocio_processor_colorspaces(
            c: *const Config,
            src: *const c_char,
            dst: *const c_char,
            err: *mut *mut c_char,
        ) -> *mut Processor;
        pub fn hz_ocio_processor_display(
            c: *const Config,
            src: *const c_char,
            display: *const c_char,
            view: *const c_char,
            inverse: c_int,
            scale: f64,
            dynamic: c_uint,
            err: *mut *mut c_char,
        ) -> *mut Processor;
        pub fn hz_ocio_processor_free(p: *mut Processor);
        pub fn hz_ocio_processor_cache_id(
            p: *const Processor,
            err: *mut *mut c_char,
        ) -> *const c_char;
        pub fn hz_ocio_processor_is_noop(p: *const Processor) -> c_int;

        pub fn hz_ocio_processor_cpu(
            p: *const Processor,
            precise: c_int,
            err: *mut *mut c_char,
        ) -> *mut Cpu;
        pub fn hz_ocio_cpu_free(c: *mut Cpu);
        pub fn hz_ocio_cpu_apply_rgba(
            c: *const Cpu,
            pixels: *mut f32,
            count: usize,
            err: *mut *mut c_char,
        ) -> c_int;
        pub fn hz_ocio_cpu_set_dynamic(
            c: *const Cpu,
            ty: c_int,
            value: f64,
            err: *mut *mut c_char,
        ) -> c_int;

        pub fn hz_ocio_processor_shader(
            p: *const Processor,
            function_name: *const c_char,
            resource_prefix: *const c_char,
            err: *mut *mut c_char,
        ) -> *mut Shader;
        pub fn hz_ocio_shader_free(s: *mut Shader);
        pub fn hz_ocio_shader_text(s: *const Shader) -> *const c_char;
        pub fn hz_ocio_shader_cache_id(s: *const Shader) -> *const c_char;
        pub fn hz_ocio_shader_texture_count(s: *const Shader) -> c_uint;
        pub fn hz_ocio_shader_texture(
            s: *const Shader,
            index: c_uint,
            out: *mut Texture,
            err: *mut *mut c_char,
        ) -> c_int;
        pub fn hz_ocio_shader_uniform_count(s: *const Shader) -> c_uint;
        pub fn hz_ocio_shader_uniform_buffer_size(s: *const Shader) -> usize;
        pub fn hz_ocio_shader_uniform(
            s: *mut Shader,
            index: c_uint,
            out: *mut Uniform,
            err: *mut *mut c_char,
        ) -> c_int;
        pub fn hz_ocio_shader_uniform_value(
            s: *const Shader,
            index: c_uint,
            out: *mut f32,
            capacity: c_int,
            err: *mut *mut c_char,
        ) -> c_int;
        pub fn hz_ocio_shader_has_dynamic(s: *const Shader, ty: c_int) -> c_int;
        pub fn hz_ocio_shader_set_dynamic(
            s: *mut Shader,
            ty: c_int,
            value: f64,
            err: *mut *mut c_char,
        ) -> c_int;
    }
}

/// What OCIO said when it refused something (the text of its exception).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Collects the error message of one call into the C++ layer.
struct ErrorSlot(*mut c_char);

impl ErrorSlot {
    fn new() -> Self {
        Self(std::ptr::null_mut())
    }

    fn as_mut(&mut self) -> *mut *mut c_char {
        &mut self.0
    }

    fn take(&mut self) -> Error {
        if self.0.is_null() {
            return Error("OpenColorIO failed without a message".into());
        }
        // SAFETY: the C++ layer stored a NUL-terminated string it allocated.
        let text = unsafe { CStr::from_ptr(self.0) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: allocated by the C++ layer with malloc, freed once.
        unsafe { ffi::hz_ocio_string_free(self.0) };
        self.0 = std::ptr::null_mut();
        Error(text)
    }
}

impl Drop for ErrorSlot {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: as in `take`.
            unsafe { ffi::hz_ocio_string_free(self.0) };
        }
    }
}

fn c_string(text: &str) -> Result<CString> {
    CString::new(text).map_err(|_| Error(format!("{text:?} contains a NUL byte")))
}

/// Copies a string the C++ layer returned. Null becomes the empty string.
///
/// # Safety
/// `p` is null or points to a NUL-terminated string that is valid now.
unsafe fn copy(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: the caller's promise.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// The version of the OCIO library that is loaded, such as `2.5.2`.
pub fn version() -> String {
    // SAFETY: returns a static string.
    unsafe { copy(ffi::hz_ocio_version()) }
}

/// Stops OCIO from writing its own log to the standard streams.
pub fn quiet() {
    // SAFETY: no arguments; the C++ layer catches everything.
    unsafe { ffi::hz_ocio_quiet() }
}

/// The names of the configs built into the library.
pub fn builtin_configs() -> Vec<String> {
    // SAFETY: plain queries of a registry that lives for the process.
    let count = unsafe { ffi::hz_ocio_builtin_count() };
    (0..count)
        .map(|i| unsafe { copy(ffi::hz_ocio_builtin_name(i)) })
        .collect()
}

/// A dynamic property: a control of a processor that can change without
/// making the processor (or its shader) again.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Dynamic {
    /// In stops, applied before the view.
    Exposure,
    Contrast,
    /// An exponent, applied after the view.
    Gamma,
}

impl Dynamic {
    fn id(self) -> c_int {
        match self {
            Self::Exposure => 0,
            Self::Contrast => 1,
            Self::Gamma => 2,
        }
    }
}

/// Whether a colour space describes the light of a scene or what a display
/// shows.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReferenceSpace {
    Scene,
    Display,
}

/// An OCIO config. Immutable once read.
pub struct Config(NonNull<ffi::Config>);

// SAFETY: the object behind it is a `ConstConfigRcPtr` (a shared pointer to
// a const config). OCIO documents const configs as safe to use from several
// threads, and the shared pointer's count is atomic.
unsafe impl Send for Config {}
unsafe impl Sync for Config {}

impl Drop for Config {
    fn drop(&mut self) {
        // SAFETY: created by hz_ocio_config_open, freed once.
        unsafe { ffi::hz_ocio_config_free(self.0.as_ptr()) }
    }
}

impl Config {
    fn open(how: c_int, spec: &str) -> Result<Self> {
        let spec = c_string(spec)?;
        let mut err = ErrorSlot::new();
        // SAFETY: valid strings; the error slot outlives the call.
        let p = unsafe { ffi::hz_ocio_config_open(how, spec.as_ptr(), err.as_mut()) };
        NonNull::new(p).map(Self).ok_or_else(|| err.take())
    }

    /// The config the environment variable `OCIO` names. When it is not set,
    /// OCIO gives a minimal config of its own.
    pub fn from_env() -> Result<Self> {
        Self::open(0, "")
    }

    /// A config file (`.ocio`, or an `.ocioz` archive), or an `ocio://` name
    /// of a built-in config, such as `ocio://studio-config-latest`.
    pub fn from_file(path: &str) -> Result<Self> {
        Self::open(1, path)
    }

    /// A config file.
    pub fn from_path(path: &Path) -> Result<Self> {
        let text = path
            .to_str()
            .ok_or_else(|| Error(format!("{} is not UTF-8", path.display())))?;
        Self::open(1, text)
    }

    /// A built-in config by its name (one of [`builtin_configs`]).
    pub fn from_builtin(name: &str) -> Result<Self> {
        Self::open(2, name)
    }

    /// `ocio://...` and paths go to [`Self::from_file`]; the empty string to
    /// [`Self::from_env`].
    pub fn from_spec(spec: &str) -> Result<Self> {
        if spec.is_empty() {
            Self::from_env()
        } else {
            Self::from_file(spec)
        }
    }

    /// A hash of everything the config's results depend on.
    pub fn cache_id(&self) -> Result<String> {
        let mut err = ErrorSlot::new();
        // SAFETY: a live config.
        let p = unsafe { ffi::hz_ocio_config_cache_id(self.0.as_ptr(), err.as_mut()) };
        if p.is_null() {
            Err(err.take())
        } else {
            // SAFETY: valid until the config is asked again; copied now.
            Ok(unsafe { copy(p) })
        }
    }

    pub fn name(&self) -> String {
        // SAFETY: a live config.
        unsafe { copy(ffi::hz_ocio_config_name(self.0.as_ptr())) }
    }

    fn list(&self, what: c_int, arg: &str) -> Vec<String> {
        let Ok(arg) = c_string(arg) else {
            return Vec::new();
        };
        // SAFETY: a live config and a valid string.
        let count = unsafe { ffi::hz_ocio_config_count(self.0.as_ptr(), what, arg.as_ptr()) };
        (0..count.max(0))
            .map(|i| unsafe {
                copy(ffi::hz_ocio_config_item(
                    self.0.as_ptr(),
                    what,
                    arg.as_ptr(),
                    i,
                ))
            })
            .collect()
    }

    /// The active colour spaces.
    pub fn color_spaces(&self) -> Vec<String> {
        self.list(0, "")
    }

    pub fn displays(&self) -> Vec<String> {
        self.list(1, "")
    }

    /// The views of a display.
    pub fn views(&self, display: &str) -> Vec<String> {
        self.list(3, display)
    }

    /// `(role, colour space)`.
    pub fn roles(&self) -> Vec<(String, String)> {
        self.list(2, "")
            .into_iter()
            .map(|role| {
                let space = self.role(&role).unwrap_or_default();
                (role, space)
            })
            .collect()
    }

    fn string_of(&self, f: impl FnOnce(*const ffi::Config) -> *const c_char) -> Option<String> {
        // SAFETY: `f` calls the C++ layer with a live config; the result is
        // copied before anything else is asked.
        let text = unsafe { copy(f(self.0.as_ptr())) };
        (!text.is_empty()).then_some(text)
    }

    /// The colour space a role names.
    pub fn role(&self, role: &str) -> Option<String> {
        let role = c_string(role).ok()?;
        self.string_of(|c| unsafe { ffi::hz_ocio_config_role(c, role.as_ptr()) })
    }

    /// The colour space that a name, an alias or a role resolves to.
    pub fn resolve(&self, name: &str) -> Option<String> {
        let name = c_string(name).ok()?;
        self.string_of(|c| unsafe { ffi::hz_ocio_config_resolve(c, name.as_ptr()) })
    }

    pub fn reference_space(&self, color_space: &str) -> Option<ReferenceSpace> {
        let name = c_string(color_space).ok()?;
        // SAFETY: a live config and a valid string.
        match unsafe { ffi::hz_ocio_config_reference_type(self.0.as_ptr(), name.as_ptr()) } {
            0 => Some(ReferenceSpace::Scene),
            1 => Some(ReferenceSpace::Display),
            _ => None,
        }
    }

    pub fn default_display(&self) -> Option<String> {
        self.string_of(|c| unsafe { ffi::hz_ocio_config_default_display(c) })
    }

    pub fn default_view(&self, display: &str) -> Option<String> {
        let display = c_string(display).ok()?;
        self.string_of(|c| unsafe { ffi::hz_ocio_config_default_view(c, display.as_ptr()) })
    }

    /// The colour space a view ends in: what the signal is after the view.
    /// For a shared view that uses the display's own colour space, OCIO
    /// answers `<USE_DISPLAY_NAME>`.
    pub fn view_color_space(&self, display: &str, view: &str) -> Option<String> {
        let display = c_string(display).ok()?;
        let view = c_string(view).ok()?;
        self.string_of(|c| unsafe {
            ffi::hz_ocio_config_view_colorspace(c, display.as_ptr(), view.as_ptr())
        })
    }

    /// The conversion from one colour space (or role) to another.
    pub fn processor(&self, src: &str, dst: &str) -> Result<Processor> {
        let (src, dst) = (c_string(src)?, c_string(dst)?);
        let mut err = ErrorSlot::new();
        // SAFETY: a live config, valid strings.
        let p = unsafe {
            ffi::hz_ocio_processor_colorspaces(
                self.0.as_ptr(),
                src.as_ptr(),
                dst.as_ptr(),
                err.as_mut(),
            )
        };
        NonNull::new(p).map(Processor).ok_or_else(|| err.take())
    }

    /// The conversion from a colour space to what a display shows through a
    /// view.
    pub fn display_processor(&self, request: &DisplayRequest<'_>) -> Result<Processor> {
        let src = c_string(request.src)?;
        let display = c_string(request.display)?;
        let view = c_string(request.view)?;
        let mut dynamic: c_uint = 0;
        if request.dynamic_exposure {
            dynamic |= 1;
        }
        if request.dynamic_gamma {
            dynamic |= 2;
        }
        let mut err = ErrorSlot::new();
        // SAFETY: a live config, valid strings.
        let p = unsafe {
            ffi::hz_ocio_processor_display(
                self.0.as_ptr(),
                src.as_ptr(),
                display.as_ptr(),
                view.as_ptr(),
                c_int::from(request.inverse),
                request.scale,
                dynamic,
                err.as_mut(),
            )
        };
        NonNull::new(p).map(Processor).ok_or_else(|| err.take())
    }
}

/// What [`Config::display_processor`] makes.
#[derive(Copy, Clone, Debug)]
pub struct DisplayRequest<'a> {
    /// The colour space (or role) of the input.
    pub src: &'a str,
    pub display: &'a str,
    pub view: &'a str,
    /// From the display's values back to the colour space.
    pub inverse: bool,
    /// Multiplies the input's RGB before anything else.
    pub scale: f64,
    /// Adds [`Dynamic::Exposure`], applied to the (scaled) input.
    pub dynamic_exposure: bool,
    /// Adds [`Dynamic::Gamma`], applied to the result of the view.
    pub dynamic_gamma: bool,
}

impl<'a> DisplayRequest<'a> {
    pub fn new(src: &'a str, display: &'a str, view: &'a str) -> Self {
        Self {
            src,
            display,
            view,
            inverse: false,
            scale: 1.0,
            dynamic_exposure: false,
            dynamic_gamma: false,
        }
    }
}

/// A conversion, not yet specialized for the CPU or the GPU.
pub struct Processor(NonNull<ffi::Processor>);

// SAFETY: a `ConstProcessorRcPtr`; OCIO's processors are const objects with
// internally locked caches.
unsafe impl Send for Processor {}
unsafe impl Sync for Processor {}

impl Drop for Processor {
    fn drop(&mut self) {
        // SAFETY: created by the C++ layer, freed once.
        unsafe { ffi::hz_ocio_processor_free(self.0.as_ptr()) }
    }
}

impl Processor {
    pub fn cache_id(&self) -> Result<String> {
        let mut err = ErrorSlot::new();
        // SAFETY: a live processor.
        let p = unsafe { ffi::hz_ocio_processor_cache_id(self.0.as_ptr(), err.as_mut()) };
        if p.is_null() {
            Err(err.take())
        } else {
            // SAFETY: copied before the processor is asked again.
            Ok(unsafe { copy(p) })
        }
    }

    pub fn is_noop(&self) -> bool {
        // SAFETY: a live processor.
        unsafe { ffi::hz_ocio_processor_is_noop(self.0.as_ptr()) != 0 }
    }

    /// The processor for the CPU, on 32-bit floats.
    ///
    /// `precise` leaves out the approximations of log, exp and pow that
    /// OCIO's default optimization uses: this is the reference the GPU
    /// results are compared with.
    pub fn cpu(&self, precise: bool) -> Result<CpuProcessor> {
        let mut err = ErrorSlot::new();
        // SAFETY: a live processor.
        let p = unsafe {
            ffi::hz_ocio_processor_cpu(self.0.as_ptr(), c_int::from(precise), err.as_mut())
        };
        NonNull::new(p).map(CpuProcessor).ok_or_else(|| err.take())
    }

    /// The processor's GPU shader, as HLSL: a function named `function`
    /// from a `float4` to a `float4`, with its resources declared at global
    /// scope under names that start with `prefix`.
    pub fn shader(&self, function: &str, prefix: &str) -> Result<ShaderDesc> {
        let (function, prefix) = (c_string(function)?, c_string(prefix)?);
        let mut err = ErrorSlot::new();
        // SAFETY: a live processor, valid strings.
        let p = unsafe {
            ffi::hz_ocio_processor_shader(
                self.0.as_ptr(),
                function.as_ptr(),
                prefix.as_ptr(),
                err.as_mut(),
            )
        };
        NonNull::new(p).map(ShaderDesc).ok_or_else(|| err.take())
    }

    /// The 3x3 matrix this processor is, when it is one: the images of the
    /// three basis vectors as columns, in row-major order. `None` when the
    /// processor is not linear (checked on a set of points) or moves black.
    pub fn as_matrix(&self) -> Result<Option<[[f64; 3]; 3]>> {
        let cpu = self.cpu(true)?;
        let mut probes: Vec<[f32; 4]> = vec![
            [0.0, 0.0, 0.0, 1.0],
            [1.0, 0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0, 1.0],
            [0.0, 0.0, 1.0, 1.0],
        ];
        let checks: [[f32; 3]; 6] = [
            [0.18, 0.18, 0.18],
            [0.3, 0.5, 0.7],
            [4.0, 2.0, 0.25],
            [0.01, 0.9, 0.02],
            [-0.2, 0.1, 0.4],
            [12.0, 0.001, 3.0],
        ];
        probes.extend(checks.iter().map(|c| [c[0], c[1], c[2], 1.0]));
        cpu.apply_rgba(&mut probes)?;
        if probes[0][..3].iter().any(|v| v.abs() > 1e-6) {
            return Ok(None);
        }
        let mut m = [[0.0f64; 3]; 3];
        for (column, probe) in probes[1..4].iter().enumerate() {
            for row in 0..3 {
                m[row][column] = f64::from(probe[row]);
            }
        }
        for (input, output) in checks.iter().zip(&probes[4..]) {
            for row in 0..3 {
                let expected: f64 = (0..3).map(|k| m[row][k] * f64::from(input[k])).sum();
                let scale = input.iter().fold(1.0f32, |a, v| a.max(v.abs()));
                if (expected - f64::from(output[row])).abs() > 2e-5 * f64::from(scale) {
                    return Ok(None);
                }
            }
        }
        Ok(Some(m))
    }
}

/// A processor specialized for the CPU.
pub struct CpuProcessor(NonNull<ffi::Cpu>);

// SAFETY: a `ConstCPUProcessorRcPtr`; applying it is a const operation that
// OCIO allows from several threads. Setting a dynamic property is not
// synchronized, so the type is `Send` but not `Sync`.
unsafe impl Send for CpuProcessor {}

impl Drop for CpuProcessor {
    fn drop(&mut self) {
        // SAFETY: created by the C++ layer, freed once.
        unsafe { ffi::hz_ocio_cpu_free(self.0.as_ptr()) }
    }
}

impl CpuProcessor {
    /// Converts RGBA pixels in place. Alpha passes through.
    pub fn apply_rgba(&self, pixels: &mut [[f32; 4]]) -> Result<()> {
        let mut err = ErrorSlot::new();
        // SAFETY: `pixels` is `len` groups of four floats, exclusively
        // borrowed for the call.
        let status = unsafe {
            ffi::hz_ocio_cpu_apply_rgba(
                self.0.as_ptr(),
                pixels.as_mut_ptr().cast(),
                pixels.len(),
                err.as_mut(),
            )
        };
        if status == 0 { Ok(()) } else { Err(err.take()) }
    }

    /// Converts one colour.
    pub fn apply(&self, rgba: [f32; 4]) -> Result<[f32; 4]> {
        let mut pixel = [rgba];
        self.apply_rgba(&mut pixel)?;
        Ok(pixel[0])
    }

    /// Changes a dynamic property. Fails when the processor has none of
    /// this kind.
    pub fn set_dynamic(&mut self, property: Dynamic, value: f64) -> Result<()> {
        let mut err = ErrorSlot::new();
        // SAFETY: a live processor, exclusively borrowed.
        let status = unsafe {
            ffi::hz_ocio_cpu_set_dynamic(self.0.as_ptr(), property.id(), value, err.as_mut())
        };
        if status == 0 { Ok(()) } else { Err(err.take()) }
    }
}

/// A lookup table of a shader, to be put in a texture.
#[derive(Clone, Debug, PartialEq)]
pub struct Texture {
    /// The name of the texture in the shader (`Texture2D` or `Texture3D`).
    pub name: String,
    /// The name of its `SamplerState` in the shader.
    pub sampler: String,
    pub width: u32,
    pub height: u32,
    /// 1 for a 2D texture.
    pub depth: u32,
    /// 1 (red) or 3 (RGB).
    pub channels: u32,
    /// Whether the sampler filters linearly (otherwise: nearest).
    pub linear: bool,
    /// `width * height * depth * channels` values, rows first, then slices.
    pub values: Vec<f32>,
}

impl Texture {
    pub fn is_3d(&self) -> bool {
        self.depth > 1
    }
}

/// The type of a uniform, as the shader declares it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UniformType {
    /// `float`.
    Float,
    /// `bool`.
    Bool,
    /// `float3`.
    Float3,
    /// An array of `float`.
    FloatArray,
    /// An array of `int`.
    IntArray,
}

/// A uniform of a shader: a value of a dynamic property.
#[derive(Clone, Debug, PartialEq)]
pub struct Uniform {
    pub name: String,
    pub ty: UniformType,
    /// Where OCIO would put it in a uniform buffer of its own layout. The
    /// compiler that builds the shader decides the real place.
    pub buffer_offset: usize,
    /// The current value (integers and booleans as floats).
    pub value: Vec<f32>,
}

/// A processor's GPU shader and what it needs.
pub struct ShaderDesc(NonNull<ffi::Shader>);

// SAFETY: a `GpuShaderDescRcPtr` that nothing else holds; moving it to
// another thread is fine. Not `Sync`: its dynamic properties are mutable.
unsafe impl Send for ShaderDesc {}

impl Drop for ShaderDesc {
    fn drop(&mut self) {
        // SAFETY: created by the C++ layer, freed once.
        unsafe { ffi::hz_ocio_shader_free(self.0.as_ptr()) }
    }
}

impl ShaderDesc {
    /// The HLSL source.
    pub fn text(&self) -> String {
        // SAFETY: a live shader description.
        unsafe { copy(ffi::hz_ocio_shader_text(self.0.as_ptr())) }
    }

    /// A hash of the shader and its resources.
    pub fn cache_id(&self) -> String {
        // SAFETY: a live shader description.
        unsafe { copy(ffi::hz_ocio_shader_cache_id(self.0.as_ptr())) }
    }

    /// The lookup tables, with their values.
    pub fn textures(&self) -> Result<Vec<Texture>> {
        // SAFETY: a live shader description.
        let count = unsafe { ffi::hz_ocio_shader_texture_count(self.0.as_ptr()) };
        let mut textures = Vec::new();
        for index in 0..count {
            let mut raw = std::mem::MaybeUninit::<ffi::Texture>::zeroed();
            let mut err = ErrorSlot::new();
            // SAFETY: `raw` is writable; the C++ layer fills every field on
            // success.
            let status = unsafe {
                ffi::hz_ocio_shader_texture(self.0.as_ptr(), index, raw.as_mut_ptr(), err.as_mut())
            };
            if status != 0 {
                return Err(err.take());
            }
            // SAFETY: filled by the call above.
            let raw = unsafe { raw.assume_init() };
            let len = raw.width as usize
                * raw.height as usize
                * raw.depth as usize
                * raw.channels as usize;
            if raw.values.is_null() || len == 0 {
                return Err(Error(format!("texture {index} has no values")));
            }
            // SAFETY: OCIO holds `len` floats there for as long as the
            // shader description lives; they are copied now.
            let values = unsafe { std::slice::from_raw_parts(raw.values, len) }.to_vec();
            textures.push(Texture {
                // SAFETY: names owned by the shader description.
                name: unsafe { copy(raw.texture_name) },
                sampler: unsafe { copy(raw.sampler_name) },
                width: raw.width,
                height: raw.height,
                depth: raw.depth,
                channels: raw.channels,
                linear: raw.linear != 0,
                values,
            });
        }
        Ok(textures)
    }

    /// The uniforms, with their current values.
    pub fn uniforms(&mut self) -> Result<Vec<Uniform>> {
        // SAFETY: a live shader description.
        let count = unsafe { ffi::hz_ocio_shader_uniform_count(self.0.as_ptr()) };
        let mut uniforms = Vec::new();
        for index in 0..count {
            let mut raw = ffi::Uniform {
                name: std::ptr::null(),
                ty: -1,
                buffer_offset: 0,
            };
            let mut err = ErrorSlot::new();
            // SAFETY: `raw` is writable, the description exclusively
            // borrowed (the call stores the name in it).
            let status = unsafe {
                ffi::hz_ocio_shader_uniform(self.0.as_ptr(), index, &mut raw, err.as_mut())
            };
            if status != 0 {
                return Err(err.take());
            }
            // SAFETY: valid until the next call of hz_ocio_shader_uniform.
            let name = unsafe { copy(raw.name) };
            let ty = match raw.ty {
                0 => UniformType::Float,
                1 => UniformType::Bool,
                2 => UniformType::Float3,
                3 => UniformType::FloatArray,
                4 => UniformType::IntArray,
                other => return Err(Error(format!("uniform `{name}` has unknown type {other}"))),
            };
            uniforms.push(Uniform {
                name,
                ty,
                buffer_offset: raw.buffer_offset,
                value: self.uniform_value(index)?,
            });
        }
        Ok(uniforms)
    }

    fn uniform_value(&self, index: c_uint) -> Result<Vec<f32>> {
        let mut value = vec![0.0f32; 16];
        loop {
            let mut err = ErrorSlot::new();
            // SAFETY: `value` has room for `len` floats.
            let n = unsafe {
                ffi::hz_ocio_shader_uniform_value(
                    self.0.as_ptr(),
                    index,
                    value.as_mut_ptr(),
                    value.len() as c_int,
                    err.as_mut(),
                )
            };
            if n < 0 {
                return Err(err.take());
            }
            let n = n as usize;
            if n <= value.len() {
                value.truncate(n);
                return Ok(value);
            }
            value.resize(n, 0.0);
        }
    }

    /// OCIO's own size for a uniform buffer of these uniforms.
    pub fn uniform_buffer_size(&self) -> usize {
        // SAFETY: a live shader description.
        unsafe { ffi::hz_ocio_shader_uniform_buffer_size(self.0.as_ptr()) }
    }

    pub fn has_dynamic(&self, property: Dynamic) -> bool {
        // SAFETY: a live shader description.
        unsafe { ffi::hz_ocio_shader_has_dynamic(self.0.as_ptr(), property.id()) != 0 }
    }

    /// Changes a dynamic property; [`Self::uniforms`] then gives the new
    /// values. The shader text does not change.
    pub fn set_dynamic(&mut self, property: Dynamic, value: f64) -> Result<()> {
        let mut err = ErrorSlot::new();
        // SAFETY: a live shader description, exclusively borrowed.
        let status = unsafe {
            ffi::hz_ocio_shader_set_dynamic(self.0.as_ptr(), property.id(), value, err.as_mut())
        };
        if status == 0 { Ok(()) } else { Err(err.take()) }
    }
}
