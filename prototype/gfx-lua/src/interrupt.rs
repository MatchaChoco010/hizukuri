//! The time budget on Luau: an interrupt installed directly in the VM.
//!
//! Luau calls its interrupt at every loop back edge and at every call and
//! return. `mlua::Lua::set_interrupt` wraps that in its machinery for Rust
//! callbacks, which keeps one preallocated error object. A Rust function
//! that fails uses that object up, and the next interrupt then makes a new
//! one on the stack of whichever script function is running, growing that
//! function's frame by two slots and shifting the whole frame to do so. The
//! frame never shrinks while the function runs, so a script that makes
//! failing calls in a loop (`pcall(gfx.destroy, nonsense)`) pays for every
//! failure in proportion to the failures before it.
//!
//! The interrupt here allocates nothing while the budget lasts: it counts,
//! looks at the clock now and then, and raises a plain Lua error when the
//! budget is spent. The host recognizes a timeout by the budget's own state,
//! not by the error value, so nothing has to travel with the error.

use std::cell::RefCell;
use std::ffi::c_int;

use mlua::{Lua, ffi};

use crate::Budget;

thread_local! {
    /// The budget of every VM of this thread that has the interrupt, by the
    /// address of the VM's main state.
    static BUDGETS: RefCell<Vec<(usize, *const Budget)>> = const { RefCell::new(Vec::new()) };
}

/// What a script that catches the error gets to see.
const MESSAGE: &[u8] = b"the script exceeded its time budget";

/// Installs the interrupt. Returns the key to [`remove`] it with.
///
/// `budget` must stay where it is until [`remove`] was called.
pub(crate) fn install(lua: &Lua, budget: *const Budget) -> mlua::Result<usize> {
    let mut key = 0;
    // SAFETY: the closure only reads the address of the main state and sets
    // a callback of the VM; it leaves the stack as it found it.
    unsafe {
        lua.exec_raw::<()>((), |state| {
            key = ffi::lua_mainthread(state) as usize;
            (*ffi::lua_callbacks(state)).interrupt = Some(interrupt);
        })?;
    }
    BUDGETS.with(|budgets| budgets.borrow_mut().push((key, budget)));
    Ok(key)
}

/// Forgets the budget of a VM. Its interrupt does nothing from then on.
pub(crate) fn remove(key: usize) {
    let _ = BUDGETS.try_with(|budgets| {
        if let Ok(mut budgets) = budgets.try_borrow_mut() {
            budgets.retain(|(k, _)| *k != key);
        }
    });
}

unsafe extern "C-unwind" fn interrupt(state: *mut ffi::lua_State, gc: c_int) {
    // A step of the collector: nothing may be raised from here.
    if gc >= 0 {
        return;
    }
    // SAFETY: `state` belongs to a live VM.
    let key = unsafe { ffi::lua_mainthread(state) } as usize;
    let spent = BUDGETS
        .try_with(|budgets| {
            let budgets = budgets.try_borrow().ok()?;
            let (_, budget) = budgets.iter().find(|(k, _)| *k == key)?;
            // SAFETY: the host keeps the budget alive while it is listed.
            Some(unsafe { (**budget).spent() })
        })
        .ok()
        .flatten()
        .unwrap_or(false);
    if spent {
        // SAFETY: one more stack slot is made sure of before the push; the
        // error unwinds through this frame, which owns nothing.
        unsafe {
            ffi::lua_rawcheckstack(state, 1);
            ffi::lua_pushlstring_(state, MESSAGE.as_ptr().cast(), MESSAGE.len());
            ffi::lua_error(state);
        }
    }
}
