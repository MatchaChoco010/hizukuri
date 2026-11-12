//! Behaviour of the script VM that the host relies on, written so that it
//! runs on every VM this crate can be built with. It records what differs
//! between Luau and Lua 5.5; only what must hold everywhere is asserted.

mod common;

use std::time::{Duration, Instant};

use common::*;
use gfx_lua::{HostOptions, ScriptError, VM_NAME};

const SCRIPT: &str = r#"
function spin() while true do end end
function spin_pcall()
    while true do
        pcall(function() while true do end end)
    end
end
function tail_recursion()
    local function again() return again() end
    return again()
end
function slow_pattern(n)
    return string.find(string.rep("a", n), string.rep("a*", n) .. "b")
end
function hoard()
    local all = {}
    while true do all[#all + 1] = { 1, 2, 3, 4 } end
end
function small() local t = { 1, 2, 3 } return #t end
function failing(n)
    local failed = 0
    for i = 1, n do
        if not pcall(gfx.destroy, 12345) then failed = failed + 1 end
    end
    assert(failed == n)
end
"#;

fn outcome(result: &Result<(), ScriptError>) -> String {
    match result {
        Ok(()) => "returned".to_owned(),
        Err(ScriptError::Timeout(_)) => "interrupted (timeout)".to_owned(),
        Err(ScriptError::Memory(_)) => "memory error".to_owned(),
        Err(ScriptError::Runtime(m)) => format!(
            "runtime error: {}",
            m.lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(70)
                .collect::<String>()
        ),
        Err(other) => format!("{other:?}"),
    }
}

#[test]
fn interrupts_memory_and_error_cost() {
    let budget = Duration::from_millis(100);
    let host = host_with(HostOptions {
        time_budget: Some(budget),
        memory_limit: Some(64 << 20),
        ..HostOptions::default()
    });
    host.load("vm.lua", SCRIPT).unwrap();
    eprintln!("VM: {VM_NAME}");

    for name in ["spin", "spin_pcall"] {
        let start = Instant::now();
        let result = host.call(name, ());
        let elapsed = start.elapsed();
        eprintln!("{name}: {} after {elapsed:?}", outcome(&result));
        assert!(
            matches!(result, Err(ScriptError::Timeout(_))),
            "{name}: {result:?}"
        );
        assert!(
            elapsed < budget + Duration::from_millis(250),
            "{name}: {elapsed:?}"
        );
    }

    // With proper tail calls this is an endless loop; without, a stack overflow.
    let start = Instant::now();
    let result = host.call("tail_recursion", ());
    eprintln!(
        "tail_recursion: {} after {:?}",
        outcome(&result),
        start.elapsed()
    );
    assert!(result.is_err());

    // One call of a native library function that runs for a long time.
    // The time grows exponentially with `n`, so stop at the first call that
    // overruns the budget: on a VM that cannot interrupt native code the next
    // one would run for minutes.
    for n in 6..=24 {
        let start = Instant::now();
        let result = host.call("slow_pattern", n);
        let elapsed = start.elapsed();
        if elapsed > budget / 4 || n == 24 {
            eprintln!(
                "slow_pattern({n}): {} after {elapsed:?} (budget {budget:?})",
                outcome(&result)
            );
        }
        if elapsed > budget + Duration::from_millis(50) {
            eprintln!("  -> a native call overran the budget; not trying larger inputs");
            break;
        }
    }

    // The rest is not about the time budget: use a generous one, so that a
    // slow (debug) build does not time out while filling the heap.
    let host = host_with(HostOptions {
        time_budget: Some(Duration::from_secs(60)),
        memory_limit: Some(64 << 20),
        ..HostOptions::default()
    });
    host.load("vm.lua", SCRIPT).unwrap();

    // Hitting the memory limit, then calling again.
    let result = host.call("hoard", ());
    eprintln!("hoard: {}", outcome(&result));
    assert!(matches!(result, Err(ScriptError::Memory(_))), "{result:?}");
    let result = host.call("small", ());
    eprintln!("small call after the memory error: {}", outcome(&result));
    assert!(result.is_ok(), "{result:?}");

    // Cost of a failing API call caught by the script.
    for n in [10, 100, 1000] {
        let start = Instant::now();
        host.call("failing", n).unwrap();
        eprintln!(
            "failing call caught by pcall, {n} in one script call: {:.1} µs each",
            start.elapsed().as_secs_f64() * 1e6 / n as f64
        );
    }
}
