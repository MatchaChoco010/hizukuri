//! Checking a module away from the device (`ShaderChecker`): the checks run
//! on another thread, the device makes the shader from their result, and
//! many checks at once give the results one check at a time gives. The
//! shaders are Slang; needs `slangc`.

use std::time::Instant;

use proto_gfx_core::*;
use proto_slangc::{Compiler, Input, Stage};

mod slang;
use slang::*;

const SHADER: &str = r#"
struct Push { float4 tint; uint index; };
[[vk::push_constant]] ConstantBuffer<Push> pc;
[[vk::binding(0, 0)]] Texture2D<float4> albedo;
[[vk::binding(1, 0)]] SamplerState smp;
struct Light { float4 position; float4 color; };
[[vk::binding(2, 0)]] StructuredBuffer<Light> lights;

[shader("vertex")]
float4 vs(uint i : SV_VulkanVertexID) : SV_Position {
    float2 p[3] = { float2(-1, -1), float2(3, -1), float2(-1, 3) };
    return float4(p[i], 0, 1);
}

[shader("fragment")]
float4 fs(float4 p : SV_Position) : SV_Target0 {
    float weights[4] = { 0.25, 0.5, 0.75, 1.0 };
    float4 sum = float4(0, 0, 0, 0);
    for (uint i = 0; i < pc.index; i++) {
        sum += lights[i].color * weights[i % 4];
    }
    return albedo.SampleLevel(smp, p.xy / 4.0, 0) * pc.tint + sum;
}
"#;

fn spirv(stage: Stage, entry: &str) -> Vec<u8> {
    Compiler::find()
        .unwrap()
        .compile(Input::Source(SHADER), stage, entry)
        .unwrap()
        .spirv
}

/// The checks run on another thread while the device stays with this one;
/// the shader made from their result is the shader the direct call makes.
#[test]
fn a_module_is_checked_on_another_thread() {
    let mut gpu = gpu();
    let fragment = spirv(Stage::Fragment, "fs");
    let checker = gpu.shader_checker();
    let (checked, checking) = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let start = Instant::now();
            let checked = checker.check(ShaderStage::Fragment, "fs", &fragment);
            (checked, start.elapsed())
        });
        // Meanwhile the device records and submits frames.
        for _ in 0..3 {
            gpu.begin_frame().unwrap();
            gpu.end_frame().unwrap();
        }
        worker.join().unwrap()
    });
    let checked = checked.unwrap();
    assert_eq!(checked.stage(), ShaderStage::Fragment);
    assert_eq!(checked.entry(), "fs");

    let start = Instant::now();
    let from_checked = gpu.create_shader_checked(checked.clone()).unwrap();
    let creating = start.elapsed();
    eprintln!("checking took {checking:?}; creating the shader from the result took {creating:?}");
    let direct = gpu
        .create_shader_spirv(ShaderStage::Fragment, "fs", &fragment)
        .unwrap();
    let reflection = |gpu: &Gpu, shader| format!("{:?}", gpu.shader_reflection(shader).unwrap());
    assert_eq!(reflection(&gpu, from_checked), reflection(&gpu, direct));
    assert_eq!(
        format!("{:?}", checked.reflection()),
        reflection(&gpu, direct)
    );

    // It draws, and it can be used again: here for a second shader.
    let vs = gpu
        .create_shader_spirv(ShaderStage::Vertex, "vs", &spirv(Stage::Vertex, "vs"))
        .unwrap();
    let again = gpu.create_shader_checked(checked).unwrap();
    for fs in [from_checked, again] {
        pipeline(
            &mut gpu,
            vs,
            fs,
            &[Format::Rgba8Unorm],
            &PipelineExtra::default(),
        )
        .unwrap();
    }

    // A checker without a device accepts what every device accepts, and its
    // result is good for this device. What the checks refuse, they refuse
    // on any thread, as an error value.
    let baseline = ShaderChecker::baseline()
        .check(ShaderStage::Fragment, "fs", &fragment)
        .unwrap();
    gpu.create_shader_checked(baseline).unwrap();
    expect!(
        checker.check(ShaderStage::Vertex, "fs", &fragment),
        ShaderCompile
    );
    expect!(
        checker.check(ShaderStage::Fragment, "fs", &fragment[..fragment.len() / 2]),
        ShaderCompile
    );
    expect!(
        checker.check(ShaderStage::Fragment, "f\0s", &fragment),
        ShaderCompile
    );
    no_validation_errors();
}

/// The module, and copies of it with one word changed: some valid, most not.
fn modules(count: usize) -> Vec<Vec<u8>> {
    let original = spirv(Stage::Fragment, "fs");
    let words = original.len() / 4;
    let mut modules = vec![original.clone()];
    for i in 0..count {
        let mut mutant = original.clone();
        // Spread over the module; flip a different bit each time.
        let at = 5 + (i * 7919) % (words - 5);
        mutant[at * 4 + (i % 4)] ^= 1 << (i % 8);
        modules.push(mutant);
    }
    modules
}

/// What a check returns, in a form that can be compared.
fn outcome(checker: &ShaderChecker, module: &[u8]) -> std::result::Result<String, String> {
    checker
        .check(ShaderStage::Fragment, "fs", module)
        .map(|checked| format!("{:?}", checked.reflection()))
        .map_err(|e| e.message)
}

fn check_concurrently(module_count: usize, threads: usize, rounds: usize) {
    let checker = ShaderChecker::baseline();
    let modules = modules(module_count);
    let expected: Vec<_> = modules.iter().map(|m| outcome(&checker, m)).collect();
    let accepted = expected.iter().filter(|e| e.is_ok()).count();
    assert!(accepted > 0 && accepted < modules.len());
    let start = Instant::now();
    std::thread::scope(|scope| {
        for thread in 0..threads {
            let (checker, modules, expected) = (&checker, &modules, &expected);
            scope.spawn(move || {
                for round in 0..rounds {
                    // Every thread walks the modules from another start, so
                    // that different modules are in the checks at one time.
                    for i in 0..modules.len() {
                        let at = (i + thread * 13 + round * 31) % modules.len();
                        assert_eq!(
                            outcome(checker, &modules[at]),
                            expected[at],
                            "module {at}, thread {thread}"
                        );
                    }
                }
            });
        }
    });
    eprintln!(
        "{} checks on {threads} threads in {:?}; {accepted} of {} modules are accepted",
        threads * rounds * modules.len(),
        start.elapsed(),
        modules.len()
    );
}

/// Checks on several threads at once give what one check at a time gives.
#[test]
fn checks_at_the_same_time_agree_with_one_at_a_time() {
    check_concurrently(60, 8, 2);
}

/// The same, for longer. Not part of the checks; run it with
///
///     cargo test -p proto-gfx-core --test checked -- --ignored many
#[test]
#[ignore]
fn many_checks_at_the_same_time() {
    check_concurrently(400, 12, 12);
}
