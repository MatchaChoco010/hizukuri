//! Builds the C++ layer (`src/shim.cpp`) and links it against the
//! OpenColorIO shared library that pkg-config finds. OpenColorIO itself is
//! not built or bundled.

fn main() {
    println!("cargo:rerun-if-changed=src/shim.cpp");
    let ocio = match pkg_config::Config::new()
        .atleast_version("2.5")
        // The link lines are printed below, after the layer that needs them.
        .cargo_metadata(false)
        .probe("OpenColorIO")
    {
        Ok(library) => library,
        Err(e) => {
            println!(
                "cargo:warning=OpenColorIO (2.5 or newer) was not found through pkg-config: {e}"
            );
            println!(
                "cargo:warning=build inside the Nix dev shell (`nix develop`), which provides it"
            );
            std::process::exit(1);
        }
    };
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .file("src/shim.cpp")
        .warnings(true);
    for dir in &ocio.include_paths {
        build.include(dir);
    }
    build.compile("hz_ocio_shim");
    for dir in &ocio.link_paths {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }
    for lib in &ocio.libs {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
