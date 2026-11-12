//! Script binding for `proto-gfx-core`.
//!
//! This crate is the only place that knows about the script language. It is
//! deliberately thin: [`api`] turns script values into `proto-gfx-core` calls, and
//! [`ScriptHost`] runs script functions under a time and memory budget and
//! turns whatever goes wrong into a [`ScriptError`].
//!
//! The VM is chosen by a cargo feature (`luau` by default). **The script
//! language of hizukuri is not decided**; this is a provisional choice.

mod api;
#[cfg(feature = "luau")]
mod interrupt;

use std::cell::{Cell, RefCell, RefMut};
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{FromLuaMulti, IntoLuaMulti, Lua};
use proto_gfx_core::{ErrorCode, GfxError, Gpu};

/// Name of the script VM this build uses.
pub const VM_NAME: &str = if cfg!(feature = "luau") {
    "Luau"
} else if cfg!(feature = "lua55") {
    "Lua 5.5"
} else {
    "LuaJIT"
};

/// What went wrong while running a script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptError {
    /// The source did not compile.
    Syntax(String),
    /// The script raised an error (including type errors in API arguments).
    Runtime(String),
    /// The GPU API rejected a call and the script did not handle it.
    Gfx(GfxError),
    /// The script ran past its time budget and was interrupted.
    Timeout(String),
    /// The script hit its memory limit.
    Memory(String),
}

impl ScriptError {
    pub fn gfx_code(&self) -> Option<ErrorCode> {
        match self {
            Self::Gfx(e) => Some(e.code),
            _ => None,
        }
    }
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Syntax(m) => write!(f, "syntax error: {m}"),
            Self::Runtime(m) => write!(f, "runtime error: {m}"),
            Self::Gfx(e) => write!(f, "gfx error: {e}"),
            Self::Timeout(m) => write!(f, "timeout: {m}"),
            Self::Memory(m) => write!(f, "out of memory: {m}"),
        }
    }
}

impl std::error::Error for ScriptError {}

/// Marker error raised inside the VM when the time budget is exhausted.
#[derive(Debug)]
struct BudgetExceeded(Duration);

impl std::fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "script exceeded its time budget of {:?}", self.0)
    }
}

impl std::error::Error for BudgetExceeded {}

impl ScriptError {
    /// Sorts an error that came out of the VM into the kinds a host handles
    /// differently.
    pub fn from_lua(error: &mlua::Error) -> Self {
        classify(error)
    }
}

fn classify(error: &mlua::Error) -> ScriptError {
    fn find(error: &mlua::Error) -> Option<ScriptError> {
        match error {
            mlua::Error::ExternalError(e) => {
                if let Some(g) = e.downcast_ref::<GfxError>() {
                    Some(ScriptError::Gfx(g.clone()))
                } else {
                    e.downcast_ref::<BudgetExceeded>()
                        .map(|b| ScriptError::Timeout(b.to_string()))
                }
            }
            mlua::Error::CallbackError { cause, .. } => find(cause),
            mlua::Error::WithContext { cause, .. } => find(cause),
            mlua::Error::MemoryError(m) => Some(ScriptError::Memory(m.clone())),
            mlua::Error::SyntaxError { message, .. } => Some(ScriptError::Syntax(message.clone())),
            _ => None,
        }
    }
    find(error).unwrap_or_else(|| ScriptError::Runtime(error.to_string()))
}

#[derive(Clone, Debug)]
pub struct HostOptions {
    /// Upper bound for the VM heap, in bytes.
    pub memory_limit: Option<usize>,
    /// Wall-clock budget for one call into the script.
    pub time_budget: Option<Duration>,
    /// Luau only: make the standard library read-only and give the script its
    /// own global table.
    pub sandbox: bool,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            memory_limit: Some(256 << 20),
            time_budget: Some(Duration::from_secs(2)),
            sandbox: true,
        }
    }
}

#[derive(Default)]
struct Budget {
    deadline: Cell<Option<Instant>>,
    limit: Cell<Duration>,
    /// Once set, every interrupt raises again, so that a `pcall` in the script
    /// cannot swallow the timeout and keep running.
    expired: Cell<bool>,
    ticks: Cell<u32>,
}

impl Budget {
    /// One tick of the VM: whether the script has to stop now.
    #[inline]
    fn spent(&self) -> bool {
        if self.expired.get() {
            return true;
        }
        let Some(deadline) = self.deadline.get() else {
            return false;
        };
        // Reading the clock is the expensive part. Luau calls this at every
        // loop iteration and function call, so look at the clock on every
        // 64th tick; a count hook is already spaced out.
        let ticks = self.ticks.get().wrapping_add(1);
        self.ticks.set(ticks);
        let look = if cfg!(feature = "luau") {
            ticks & 63 == 0
        } else {
            true
        };
        if look && Instant::now() >= deadline {
            self.expired.set(true);
        }
        self.expired.get()
    }

    /// [`Self::spent`] for a hook that reports through `mlua`.
    #[cfg(not(feature = "luau"))]
    fn check(&self) -> mlua::Result<mlua::VmState> {
        if self.spent() {
            return Err(mlua::Error::external(BudgetExceeded(self.limit.get())));
        }
        Ok(mlua::VmState::Continue)
    }
}

/// Instructions between two checks of the budget on VMs with a count hook.
#[cfg(not(feature = "luau"))]
const COUNT_HOOK_INTERVAL: u32 = 256;

/// Installs the count hook of the PUC Lua family.
///
/// A count hook that fires every N instructions is not enough once the budget
/// is exhausted: `while true do pcall(function() while true do end end) end`
/// spends nearly all its instructions inside the protected function, so the
/// error is raised there and swallowed, forever. After expiry the hook is
/// therefore re-installed to fire on every instruction, which raises in the
/// unprotected caller as soon as the `pcall` returns.
#[cfg(not(feature = "luau"))]
fn install_count_hook(lua: &Lua, budget: &Rc<Budget>, interval: u32) -> mlua::Result<()> {
    let b = Rc::clone(budget);
    lua.set_global_hook(
        mlua::HookTriggers::new().every_nth_instruction(interval),
        move |lua, _| {
            let was_expired = b.expired.get();
            let result = b.check();
            if result.is_err() && !was_expired {
                install_count_hook(lua, &b, 1)?;
            }
            result
        },
    )
}

/// A script VM wired to one [`Gpu`].
pub struct ScriptHost {
    lua: Lua,
    gpu: Rc<RefCell<Gpu>>,
    budget: Rc<Budget>,
    options: HostOptions,
    /// The key the VM's interrupt was installed with, if it has one.
    #[cfg(feature = "luau")]
    interrupt: Option<usize>,
}

#[cfg(feature = "luau")]
impl Drop for ScriptHost {
    fn drop(&mut self) {
        if let Some(key) = self.interrupt.take() {
            interrupt::remove(key);
        }
    }
}

impl ScriptHost {
    pub fn new(gpu: Gpu, options: HostOptions) -> Result<Self, ScriptError> {
        Self::with_setup(gpu, options, |_, _| Ok(()))
    }

    /// Like [`ScriptHost::new`]. `setup` runs after the `gfx` table exists
    /// and before the VM is sandboxed: the place for a host to install more
    /// globals that scripts cannot replace. It gets the shared GPU so that
    /// host functions can use it.
    pub fn with_setup(
        gpu: Gpu,
        options: HostOptions,
        setup: impl FnOnce(&Lua, &Rc<RefCell<Gpu>>) -> mlua::Result<()>,
    ) -> Result<Self, ScriptError> {
        // No `io`, `os`, `debug` or `package`: the spike gives scripts the GPU
        // API and pure computation only.
        let libs = mlua::StdLib::TABLE
            | mlua::StdLib::STRING
            | mlua::StdLib::MATH
            | mlua::StdLib::COROUTINE;
        #[cfg(feature = "luau")]
        let libs = libs | mlua::StdLib::BUFFER | mlua::StdLib::VECTOR | mlua::StdLib::BIT;
        let lua = Lua::new_with(libs, mlua::LuaOptions::default()).map_err(|e| classify(&e))?;
        let gpu = Rc::new(RefCell::new(gpu));
        api::register(&lua, &gpu).map_err(|e| classify(&e))?;
        setup(&lua, &gpu).map_err(|e| classify(&e))?;

        let budget = Rc::new(Budget::default());
        #[cfg(feature = "luau")]
        let interrupt = match options.time_budget {
            Some(_) => {
                Some(interrupt::install(&lua, Rc::as_ptr(&budget)).map_err(|e| classify(&e))?)
            }
            None => None,
        };
        #[cfg(not(feature = "luau"))]
        if options.time_budget.is_some() {
            install_count_hook(&lua, &budget, COUNT_HOOK_INTERVAL).map_err(|e| classify(&e))?;
        }
        // From here on the host exists, so that leaving early takes the
        // interrupt's registration with it.
        let host = Self {
            lua,
            gpu,
            budget,
            options,
            #[cfg(feature = "luau")]
            interrupt,
        };
        if let Some(limit) = host.options.memory_limit {
            host.lua.set_memory_limit(limit).map_err(|e| classify(&e))?;
        }
        #[cfg(feature = "luau")]
        if host.options.sandbox {
            host.lua.sandbox(true).map_err(|e| classify(&e))?;
        }
        Ok(host)
    }

    pub fn gpu(&self) -> RefMut<'_, Gpu> {
        self.gpu.borrow_mut()
    }

    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    fn with_budget<T>(&self, f: impl FnOnce() -> mlua::Result<T>) -> Result<T, ScriptError> {
        self.with_budget_of(self.options.time_budget, f)
    }

    fn with_budget_of<T>(
        &self,
        budget: Option<Duration>,
        f: impl FnOnce() -> mlua::Result<T>,
    ) -> Result<T, ScriptError> {
        // A host that was created without a budget has no interrupt
        // installed, so a per-call budget cannot be enforced either.
        let budget = self.options.time_budget.and(budget);
        if let Some(limit) = budget {
            self.budget.limit.set(limit);
            self.budget.expired.set(false);
            self.budget.deadline.set(Some(Instant::now() + limit));
        }
        let result = f();
        self.budget.deadline.set(None);
        let expired = self.budget.expired.replace(false);
        #[cfg(not(feature = "luau"))]
        if expired {
            // Back from the per-instruction hook to the cheap one.
            let _ = install_count_hook(&self.lua, &self.budget, COUNT_HOOK_INTERVAL);
        }
        // A call that failed after its budget ran out failed because of
        // that, whatever the error it came back with looks like.
        let result = result.map_err(|e| {
            if expired {
                ScriptError::Timeout(BudgetExceeded(self.budget.limit.get()).to_string())
            } else {
                classify(&e)
            }
        });
        if matches!(result, Err(ScriptError::Memory(_))) {
            // Luau does not run an emergency collection when an allocation
            // hits the limit, so the heap stays full of what the failed call
            // left behind and the very next allocation would fail as well.
            let _ = self.lua.gc_collect();
            let _ = self.lua.gc_collect();
        }
        result
    }

    /// Compiles and runs a chunk. The chunk usually defines `init` and `frame`.
    pub fn load(&self, name: &str, source: &str) -> Result<(), ScriptError> {
        self.load_within(self.options.time_budget, name, source)
    }

    /// Like [`ScriptHost::load`]. `budget` replaces the host's default time
    /// budget for this chunk.
    pub fn load_within(
        &self,
        budget: Option<Duration>,
        name: &str,
        source: &str,
    ) -> Result<(), ScriptError> {
        self.with_budget_of(budget.or(self.options.time_budget), || {
            self.lua.load(source).set_name(name).exec()
        })
    }

    pub fn has_function(&self, name: &str) -> bool {
        self.lua
            .globals()
            .get::<Option<mlua::Function>>(name)
            .is_ok_and(|f| f.is_some())
    }

    /// Calls a global script function under the time budget.
    pub fn call<A: IntoLuaMulti>(&self, name: &str, args: A) -> Result<(), ScriptError> {
        self.with_budget(|| {
            let function: mlua::Function = self.lua.globals().get(name)?;
            function.call::<()>(args)
        })
    }

    /// Calls a global script function and returns what it returns. `budget`
    /// replaces the host's default time budget for this call.
    pub fn call_function<A: IntoLuaMulti, R: FromLuaMulti>(
        &self,
        name: &str,
        args: A,
        budget: Option<Duration>,
    ) -> Result<R, ScriptError> {
        self.with_budget_of(budget.or(self.options.time_budget), || {
            let function: mlua::Function = self.lua.globals().get(name)?;
            function.call::<R>(args)
        })
    }

    /// Runs `f` against the VM under a time budget and sorts whatever error
    /// comes out. For hosts that need more than calling a global by name
    /// (scoped callbacks, loading chunks with a longer budget).
    pub fn run<T>(
        &self,
        budget: Option<Duration>,
        f: impl FnOnce(&Lua) -> mlua::Result<T>,
    ) -> Result<T, ScriptError> {
        self.with_budget_of(budget.or(self.options.time_budget), || f(&self.lua))
    }

    /// Drops what the script defined (its global table), keeping the VM, the
    /// host's globals and every GPU object. See [`ScriptHost::reset`].
    pub fn reset_globals(&self) -> Result<(), ScriptError> {
        #[cfg(feature = "luau")]
        if self.options.sandbox {
            self.lua.sandbox(false).map_err(|e| classify(&e))?;
            self.lua.sandbox(true).map_err(|e| classify(&e))?;
        }
        Ok(())
    }

    /// The shared GPU, for host functions that outlive a borrow.
    pub fn shared_gpu(&self) -> Rc<RefCell<Gpu>> {
        Rc::clone(&self.gpu)
    }

    /// Calls a script function with a frame open around it.
    ///
    /// The frame is always ended and submitted, also when the script fails
    /// half-way (an open render pass is closed first). The first error is
    /// returned.
    pub fn call_in_frame<A: IntoLuaMulti>(&self, name: &str, args: A) -> Result<(), ScriptError> {
        self.call_in_frame_within(None, name, args)
    }

    /// Like [`ScriptHost::call_in_frame`]. `budget` replaces the host's
    /// default time budget for this call.
    pub fn call_in_frame_within<A: IntoLuaMulti>(
        &self,
        budget: Option<Duration>,
        name: &str,
        args: A,
    ) -> Result<(), ScriptError> {
        self.gpu
            .borrow_mut()
            .begin_frame()
            .map_err(ScriptError::Gfx)?;
        let script = self.call_function::<A, ()>(name, args, budget);
        let end = self.gpu.borrow_mut().end_frame();
        script?;
        end.map_err(ScriptError::Gfx)
    }

    /// Forgets everything the script defined and destroys every GPU object it
    /// created, keeping the VM and the device. The next [`ScriptHost::load`]
    /// starts from a clean slate.
    pub fn reset(&self) -> Result<(), ScriptError> {
        self.gpu.borrow_mut().destroy_all();
        #[cfg(feature = "luau")]
        if self.options.sandbox {
            // Leaving the sandbox drops the script's global table.
            self.lua.sandbox(false).map_err(|e| classify(&e))?;
            self.lua.sandbox(true).map_err(|e| classify(&e))?;
        }
        Ok(())
    }
}
