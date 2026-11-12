//! Script-side failures: syntax errors, exceptions, type errors, endless
//! loops, memory exhaustion. Each must surface as a `ScriptError`, leave the
//! process running, and leave the device able to render the next frame.
#![cfg(feature = "luau")]

mod common;

use std::time::{Duration, Instant};

use common::*;
use proto_gfx_core::{ErrorCode, Format, Handle};
use proto_gfx_lua::{HostOptions, ScriptError, ScriptHost};

/// A host with the triangle example loaded, and a target to render into.
fn triangle_host(options: HostOptions) -> (ScriptHost, Handle) {
    let host = host_with(options);
    load_example(&host, "triangle.luau");
    let output = target(&host, Format::Rgba8Unorm, 64, 64);
    (host, output)
}

/// Renders the triangle and checks it: the "next frame" after a failure.
fn next_frame_is_correct(host: &ScriptHost, output: Handle) {
    host.call_in_frame("frame", output.to_bits())
        .expect("next frame");
    let pixels = read(host, output);
    let tolerance = UNORM8_CONVERSION;
    assert_close8(
        px8(&pixels, 64, 32, 20),
        [255, 128, 64, 255],
        tolerance,
        "inside",
    );
    assert_close8(
        px8(&pixels, 64, 2, 2),
        [0, 0, 128, 255],
        tolerance,
        "outside",
    );
}

fn runtime_message(result: Result<(), ScriptError>) -> String {
    match result {
        Err(ScriptError::Runtime(message)) => message,
        other => panic!("expected a runtime error, got {other:?}"),
    }
}

#[test]
fn syntax_errors() {
    let (host, output) = triangle_host(patient());
    for source in [
        "function frame( end",
        "local = 3",
        "x = = 1",
        "return )",
        "\u{0}\u{1}\u{2}",
        "for i = 1 do end",
    ] {
        match host.load("broken.luau", source) {
            Err(ScriptError::Syntax(message)) => assert!(!message.is_empty()),
            other => panic!("{source:?}: expected a syntax error, got {other:?}"),
        }
    }
    // The previously loaded script is untouched.
    next_frame_is_correct(&host, output);
    assert_no_validation_errors();
}

#[test]
fn exceptions_and_type_errors() {
    let (host, output) = triangle_host(patient());
    host.load(
        "faulty.luau",
        r#"
        function throws_string() error("boom") end
        function throws_table() error({ code = 42 }) end
        function throws_nil() error(nil) end
        function calls_nil() local t = {} t.missing() end
        function indexes_nil() local t = nil return t.field end
        function arithmetic() return {} + 1 end
        function recursion() return recursion() + 1 end
        -- Wrong argument types for the GPU API.
        function draw_string() gfx.draw("three") end
        function draw_nil() gfx.draw(nil) end
        function draw_negative() gfx.draw(-3) end
        function buffer_not_table() gfx.create_buffer(42) end
        function buffer_bad_field() gfx.create_buffer({ size = "big", usage = { "vertex" } }) end
        function buffer_bad_usage() gfx.create_buffer({ size = 16, usage = { "teapot" } }) end
        function buffer_no_usage() gfx.create_buffer({ size = 16 }) end
        function handle_is_table() gfx.destroy({}) end
        function handle_is_fraction() gfx.destroy(1.5) end
        function handle_is_huge() gfx.destroy(1e300) end
        function shader_no_source() gfx.create_shader({ stage = "vertex" }) end
        function shader_bad_stage() gfx.create_shader({ stage = "geometry", source = "" }) end
        function pass_no_table() gfx.begin_pass(nil) end
        function pass_bad_load() gfx.begin_pass({ color = { { image = 0, load = "maybe" } } }) end
        function push_table() gfx.push(0, {}) end
        "#,
    )
    .unwrap();

    assert!(runtime_message(host.call_in_frame("throws_string", ())).contains("boom"));
    runtime_message(host.call_in_frame("throws_table", ()));
    runtime_message(host.call_in_frame("throws_nil", ()));
    assert!(runtime_message(host.call_in_frame("calls_nil", ())).contains("nil"));
    assert!(runtime_message(host.call_in_frame("indexes_nil", ())).contains("nil"));
    runtime_message(host.call_in_frame("arithmetic", ()));
    assert!(runtime_message(host.call_in_frame("recursion", ())).contains("stack overflow"));
    runtime_message(host.call_in_frame("no_such_function", ()));

    for name in [
        "draw_string",
        "draw_nil",
        "draw_negative",
        "buffer_not_table",
        "handle_is_table",
        "handle_is_fraction",
        "handle_is_huge",
        "pass_no_table",
    ] {
        let message = runtime_message(host.call_in_frame(name, ()));
        assert!(!message.is_empty(), "{name}");
    }
    for name in [
        "buffer_bad_field",
        "buffer_bad_usage",
        "buffer_no_usage",
        "shader_no_source",
        "shader_bad_stage",
        "pass_bad_load",
        "push_table",
    ] {
        let result = host.call_in_frame(name, ());
        assert_eq!(
            result.as_ref().err().and_then(ScriptError::gfx_code),
            Some(ErrorCode::InvalidArgument),
            "{name}: {result:?}"
        );
    }
    next_frame_is_correct(&host, output);
    assert_no_validation_errors();
}

/// A script throws while a render pass is open. The host still ends the
/// frame; what was recorded so far is executed; the next frame is normal.
#[test]
fn exception_in_the_middle_of_a_pass() {
    let (host, output) = triangle_host(patient());
    host.load(
        "midpass.luau",
        r#"
        function dies_in_pass(target)
            gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 1, 0, 0, 1 } } } })
            error("gave up after begin_pass")
        end
        function dies_after_gfx_error(target)
            gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 0, 1, 0, 1 } } } })
            gfx.draw(3) -- no pipeline: the API raises, nobody catches
        end
        function forgets_end_pass(target)
            gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 0, 0, 1, 1 } } } })
        end
        "#,
    )
    .unwrap();

    assert!(
        runtime_message(host.call_in_frame("dies_in_pass", output.to_bits())).contains("gave up")
    );
    assert!(!host.gpu().is_recording());
    // The clear that was recorded before the error did run.
    let pixels = read(&host, output);
    assert_eq!(px8(&pixels, 64, 5, 5), [255, 0, 0, 255]);
    next_frame_is_correct(&host, output);

    let result = host.call_in_frame("dies_after_gfx_error", output.to_bits());
    assert_eq!(
        result.as_ref().err().and_then(ScriptError::gfx_code),
        Some(ErrorCode::InvalidState),
        "{result:?}"
    );
    assert_eq!(px8(&read(&host, output), 64, 5, 5), [0, 255, 0, 255]);
    next_frame_is_correct(&host, output);

    // No exception at all, but the pass was left open.
    let result = host.call_in_frame("forgets_end_pass", output.to_bits());
    assert_eq!(
        result.as_ref().err().and_then(ScriptError::gfx_code),
        Some(ErrorCode::InvalidState),
        "{result:?}"
    );
    assert_eq!(px8(&read(&host, output), 64, 5, 5), [0, 0, 255, 255]);
    next_frame_is_correct(&host, output);
    assert_no_validation_errors();
}

/// A script may handle GPU errors itself and carry on within the same frame.
#[test]
fn scripts_can_catch_gpu_errors() {
    let (host, output) = triangle_host(patient());
    host.load(
        "careful.luau",
        r#"
        caught = {}
        function careful(target)
            local ok, err = pcall(gfx.destroy, 12345)
            table.insert(caught, tostring(err))
            ok, err = pcall(gfx.write_buffer, target, 0, "abcd")
            table.insert(caught, tostring(err))
            ok, err = pcall(gfx.create_shader, { stage = "fragment", source = "fn main( {" })
            table.insert(caught, tostring(err))
            gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 0, 0, 0, 1 } } } })
            ok, err = pcall(gfx.draw, 3)
            table.insert(caught, tostring(err))
            gfx.end_pass()
            assert(not ok)
        end
        "#,
    )
    .unwrap();
    host.call_in_frame("careful", output.to_bits()).unwrap();
    let caught: Vec<String> = host.lua().globals().get("caught").unwrap();
    assert_eq!(caught.len(), 4);
    assert!(caught[0].contains("InvalidHandle"), "{}", caught[0]);
    assert!(caught[1].contains("WrongKind"), "{}", caught[1]);
    assert!(caught[2].contains("ShaderCompile"), "{}", caught[2]);
    assert!(caught[3].contains("InvalidState"), "{}", caught[3]);
    next_frame_is_correct(&host, output);
    assert_no_validation_errors();
}

/// Calls a function that never returns under a short budget of its own.
/// It is interrupted, not before the budget, and not long after.
fn expect_timeout(host: &ScriptHost, name: &str, args: impl mlua::IntoLuaMulti, budget: Duration) {
    let start = Instant::now();
    let result = host.call_in_frame_within(Some(budget), name, args);
    let elapsed = start.elapsed();
    assert!(
        matches!(result, Err(ScriptError::Timeout(_))),
        "{name}: expected a timeout, got {result:?}"
    );
    eprintln!("{name}: interrupted after {elapsed:?} (budget {budget:?})");
    assert!(elapsed >= budget, "{name}: returned before the budget");
    assert!(
        elapsed < budget + STOPS_WITHIN,
        "{name}: took {elapsed:?} to stop"
    );
}

/// Loops that never end are cut off, and the next frame goes out.
#[test]
fn endless_loops_are_interrupted() {
    // Everything that is expected to return runs under the patient budget
    // of the host; only the loops get this one.
    let budget = Duration::from_millis(100);
    let (host, output) = triangle_host(patient());
    host.load(
        "endless.luau",
        r#"
        function spin() while true do end end
        function spin_calls() local f = function() end while true do f() end end
        -- Luau has no proper tail calls: this overflows the stack instead of
        -- running forever.
        function tail_recursion()
            local function again() return again() end
            again()
        end
        -- Tries to swallow the interruption.
        function spin_pcall()
            while true do
                pcall(function() while true do end end)
            end
        end
        function spin_nested_pcall()
            pcall(function()
                while true do
                    pcall(function() while true do pcall(error, "x") end end)
                end
            end)
            while true do end
        end
        function spin_coroutine()
            local co = coroutine.wrap(function() while true do coroutine.yield() end end)
            while true do co() end
        end
        function spin_in_pass(target)
            gfx.begin_pass({ color = { { image = target, load = "clear", clear = { 1, 1, 0, 1 } } } })
            while true do end
        end
        function spin_gfx_calls()
            while true do gfx.noop(1, 2) end
        end
        "#,
    )
    .unwrap();

    for name in [
        "spin",
        "spin_calls",
        "spin_pcall",
        "spin_nested_pcall",
        "spin_coroutine",
        "spin_gfx_calls",
    ] {
        expect_timeout(&host, name, (), budget);
        next_frame_is_correct(&host, output);
    }
    assert!(runtime_message(host.call_in_frame("tail_recursion", ())).contains("stack overflow"));
    next_frame_is_correct(&host, output);
    // Stuck with a render pass open: the frame is closed and submitted, and
    // the following frame is correct.
    expect_timeout(&host, "spin_in_pass", output.to_bits(), budget);
    assert_eq!(px8(&read(&host, output), 64, 5, 5), [255, 255, 0, 255]);
    next_frame_is_correct(&host, output);

    // A chunk that never finishes loading.
    let start = Instant::now();
    let result = host.load_within(Some(budget), "spin.luau", "while true do end");
    assert!(matches!(result, Err(ScriptError::Timeout(_))), "{result:?}");
    assert!(start.elapsed() >= budget);
    assert!(start.elapsed() < budget + STOPS_WITHIN);
    next_frame_is_correct(&host, output);
    assert_no_validation_errors();
}

/// Time spent inside one call of a native library function. Whether the
/// interrupt reaches it depends on the VM: Luau polls the interrupt inside its
/// pattern matcher, so the call below stops at the budget. This test records
/// what happens rather than asserting it.
#[test]
fn native_calls_and_the_budget() {
    let budget = Duration::from_millis(50);
    let (host, output) = triangle_host(patient());
    host.load(
        "native.luau",
        r#"
        -- A pattern that backtracks exponentially before failing to match.
        function slow_pattern(n)
            return string.find(string.rep("a", n), string.rep("a*", n) .. "b")
        end
        "#,
    )
    .unwrap();
    let mut report = Vec::new();
    for n in [16, 18, 20, 22, 24] {
        let start = Instant::now();
        let result = host.call_function::<_, ()>("slow_pattern", n, Some(budget));
        let elapsed = start.elapsed();
        report.push(format!(
            "n={n}: {elapsed:?} -> {}",
            match &result {
                Ok(()) => "returned normally".to_owned(),
                Err(e) => format!("{e}").chars().take(60).collect(),
            }
        ));
        if elapsed > Duration::from_secs(4) {
            break;
        }
    }
    eprintln!(
        "native call vs a {budget:?} budget:\n  {}",
        report.join("\n  ")
    );
    next_frame_is_correct(&host, output);
    assert_no_validation_errors();
}

#[test]
fn memory_limit() {
    let (host, output) = triangle_host(HostOptions {
        memory_limit: Some(64 << 20),
        ..patient()
    });
    host.load(
        "hungry.luau",
        r#"
        function hoard_tables()
            local all = {}
            while true do all[#all + 1] = { 1, 2, 3, 4 } end
        end
        function hoard_strings()
            local all = {}
            local i = 0
            while true do
                i += 1
                all[i] = string.rep("x", 4096) .. i
            end
        end
        function one_huge_string() return string.rep("x", 2 ^ 28) end
        function one_huge_buffer() return buffer.create(2 ^ 28) end
        kept = nil
        function hoard_and_keep()
            kept = {}
            while true do kept[#kept + 1] = { 1, 2, 3, 4 } end
        end
        "#,
    )
    .unwrap();
    for name in [
        "hoard_tables",
        "hoard_strings",
        "one_huge_string",
        "one_huge_buffer",
    ] {
        let start = Instant::now();
        let result = host.call_in_frame(name, ());
        assert!(
            matches!(
                result,
                Err(ScriptError::Memory(_)) | Err(ScriptError::Runtime(_))
            ),
            "{name}: {result:?}"
        );
        eprintln!(
            "{name}: {:?} after {:?}",
            result.unwrap_err(),
            start.elapsed()
        );
        assert!(host.lua().used_memory() <= 64 << 20);
        next_frame_is_correct(&host, output);
    }
    // If what the script hoarded stays reachable, collecting does not help:
    // the heap is pinned at the limit and even a frame that allocates one
    // small table fails. The only way out is to drop the script's state.
    let result = host.call_in_frame("hoard_and_keep", ());
    assert!(matches!(result, Err(ScriptError::Memory(_))), "{result:?}");
    eprintln!("kept heap: {} MiB", host.lua().used_memory() >> 20);
    let result = host.call_in_frame("frame", output.to_bits());
    assert!(matches!(result, Err(ScriptError::Memory(_))), "{result:?}");
    host.reset().unwrap();
    host.lua().gc_collect().unwrap();
    host.lua().gc_collect().unwrap();
    eprintln!("after reset: {} KiB", host.lua().used_memory() >> 10);
    assert!(host.lua().used_memory() < 8 << 20);
    load_example(&host, "triangle.luau");
    let output = target(&host, Format::Rgba8Unorm, 64, 64);
    next_frame_is_correct(&host, output);
    assert_no_validation_errors();
}

/// A script creates GPU objects without end.
#[test]
fn script_creates_resources_forever() {
    let (host, output) = triangle_host(patient());
    host.load(
        "greedy.luau",
        r#"
        function greedy_images()
            while true do
                gfx.create_image({ format = "rgba16float", width = 1024, height = 1024, usage = { "sampled" } })
            end
        end
        function greedy_slots(image)
            while true do gfx.create_texture_slot(image) end
        end
        function greedy_buffers()
            while true do gfx.create_buffer({ size = 64, usage = { "vertex" } }) end
        end
        "#,
    )
    .unwrap();
    for (name, argument) in [
        ("greedy_images", None),
        ("greedy_slots", Some(output.to_bits())),
        ("greedy_buffers", None),
    ] {
        let result = host.call_in_frame(name, argument);
        assert_eq!(
            result.as_ref().err().and_then(ScriptError::gfx_code),
            Some(ErrorCode::LimitExceeded),
            "{name}: {result:?}"
        );
        eprintln!("{name}: {}", result.unwrap_err());
    }
    let stats = host.gpu().stats();
    eprintln!("after the greedy scripts: {stats:?}");
    assert!(stats.memory_bytes <= 1 << 30);
    // The host's own target was created before the script filled the budget
    // and still works. Everything the script made can be dropped at once.
    let pixels_before = read(&host, output);
    host.reset().unwrap();
    host.gpu().wait_idle().unwrap();
    let stats = host.gpu().stats();
    assert_eq!(
        (stats.live_images, stats.live_buffers, stats.live_slots),
        (0, 0, 0)
    );
    assert_eq!(stats.memory_bytes, 0);
    drop(pixels_before);
    // `reset` also destroyed the host's target: host and script objects are
    // not told apart in this spike. Start over.
    load_example(&host, "triangle.luau");
    let output = target(&host, Format::Rgba8Unorm, 64, 64);
    next_frame_is_correct(&host, output);
    assert_no_validation_errors();
}

/// A script that passes every number it can think of as a handle.
#[test]
fn fabricated_handles() {
    let (host, output) = triangle_host(patient());
    host.load(
        "forger.luau",
        r#"
        function forge(target)
            local failures = 0
            local candidates = { 0, 1, -1, 2 ^ 31, 2 ^ 52, 2 ^ 53, 2 ^ 63, -2 ^ 63, target + 1, target - 1, target + 2 ^ 20 }
            for i = 1, 300 do candidates[#candidates + 1] = i * 7919 end
            for _, value in candidates do
                for _, call in { gfx.destroy, gfx.set_pipeline, gfx.create_texture_slot, gfx.image_info } do
                    if not pcall(call, value) then failures += 1 end
                end
                if not pcall(gfx.bind, 0, value) then failures += 1 end
                if not pcall(gfx.write_buffer, value, 0, "abcd") then failures += 1 end
                if not pcall(gfx.set_vertex_buffer, 0, value) then failures += 1 end
            end
            return failures
        end
        function run(target) rejected = forge(target) end
        "#,
    )
    .unwrap();
    host.call_in_frame("run", output.to_bits()).unwrap();
    let rejected: u32 = host.lua().globals().get("rejected").unwrap();
    assert_eq!(rejected, (11 + 300) * 7);
    next_frame_is_correct(&host, output);
    assert_no_validation_errors();
}

/// CPU time this thread has used. Unlike the wall clock it does not move
/// while the thread waits for a busy machine.
fn cpu_time() -> Duration {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
    Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
}

/// A failing API call that the script catches costs the same whether it is
/// the first of a call or the ten thousandth. (With the time budget
/// installed through the VM binding's own interrupt it did not: every
/// failure grew the stack frame of the running function, and each one cost
/// in proportion to the failures before it.)
#[test]
fn caught_errors_cost_the_same_however_many() {
    let (host, output) = triangle_host(patient());
    host.load(
        "many.luau",
        r#"
        function failing(n)
            for i = 1, n do
                pcall(gfx.destroy, 12345)
            end
        end
        "#,
    )
    .unwrap();
    // The fastest of three, in CPU time: what the work costs, not what else
    // the machine was doing.
    let cost = |n: u32| {
        (0..3)
            .map(|_| {
                let start = cpu_time();
                host.call_in_frame("failing", n).unwrap();
                cpu_time() - start
            })
            .min()
            .unwrap()
    };
    let (few, many) = (cost(500), cost(8000));
    eprintln!("500 caught failures: {few:?}, 8000: {many:?}");
    // 16 times the failures take about 16 times as long. If each one cost in
    // proportion to those before it, they would take 256 times as long. The
    // bound is far from both.
    assert!(
        many < few * 64,
        "8000 failures took {many:?}, 500 took {few:?}"
    );
    next_frame_is_correct(&host, output);
    assert_no_validation_errors();
}

/// The device is lost under a running script; the host builds a new device
/// and the same script source sets itself up again.
#[test]
fn device_loss_seen_from_a_script() {
    let (host, output) = triangle_host(patient());
    next_frame_is_correct(&host, output);
    host.gpu().simulate_device_loss();
    let result = host.call_in_frame("frame", output.to_bits());
    assert_eq!(
        result.as_ref().err().and_then(ScriptError::gfx_code),
        Some(ErrorCode::DeviceLost),
        "{result:?}"
    );
    drop(host);
    let (host, output) = triangle_host(patient());
    next_frame_is_correct(&host, output);
    assert_no_validation_errors();
}

/// Replacing a running script: the old script's functions and GPU objects are
/// gone, the new one starts clean.
#[test]
fn reload_replaces_the_script() {
    let (host, _) = triangle_host(patient());
    assert!(host.has_function("frame"));
    let before = host.gpu().stats();
    assert!(before.live_buffers >= 2 && before.live_pipelines == 1);

    host.reset().unwrap();
    assert!(!host.has_function("frame"), "old globals must be gone");
    assert!(host.has_function("print") || host.lua().globals().contains_key("gfx").unwrap());
    let stats = host.gpu().stats();
    assert_eq!(
        (stats.live_buffers, stats.live_pipelines, stats.live_images),
        (0, 0, 0)
    );

    load_example(&host, "triangle.luau");
    let output = target(&host, Format::Rgba8Unorm, 64, 64);
    next_frame_is_correct(&host, output);
    assert_no_validation_errors();
}
