{
  description = "Development environment for hizukuri";

  inputs = {
    # flake.lock pins the commit. Mesa (lavapipe and the hardware drivers),
    # LLVM, the libraries and the test clients all come from here.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    # Builds the toolchain that rust-toolchain.toml asks for, so the Rust
    # version is decided in one place only.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      inherit (pkgs) lib;

      # rust-toolchain.toml decides the channel and the components. The
      # flake only adds what a shell needs on top (rustup would install the
      # "default" profile, which carries the offline documentation; skip it).
      toolchainFile = (lib.importTOML ./rust-toolchain.toml).toolchain;
      rustToolchain =
        extraComponents:
        pkgs.rust-bin.fromRustupToolchain (
          toolchainFile
          // {
            profile = toolchainFile.profile or "minimal";
            components = (toolchainFile.components or [ ]) ++ extraComponents;
          }
        );

      # Libraries the build links against: Smithay and the KMS backend.
      buildLibs = with pkgs; [
        wayland
        libxkbcommon
        libinput
        seatd # libseat
        udev # libudev
        libdrm
        libgbm
        # The libdisplay-info crate that Smithay uses accepts the 0.3 series
        # of the C library, not the 0.4 that nixpkgs ships as the default.
        libdisplay-info_0_3
        vulkan-loader
        # OpenColorIO, linked as a shared library (prototype/ocio finds it
        # through pkg-config).
        opencolorio
      ];

      # Libraries that are opened with dlopen() at run time. They are put on
      # the RUNPATH of everything linked in the shell (see NIX_LDFLAGS), so
      # the binaries find them without LD_LIBRARY_PATH.
      dlopenLibs = with pkgs; [
        vulkan-loader # vulkan-rs loads libvulkan.so.1 through libloading
        wayland
        libxkbcommon
      ];

      # Cargo does not know which libraries the linker picks up, so a
      # target directory must not be reused when they change: not between a
      # build outside the shell and one inside, and not across a flake.lock
      # update. The directory name therefore carries a hash of everything
      # that is linked in.
      linkHash = builtins.substring 0 12 (
        builtins.hashString "sha256" (
          builtins.unsafeDiscardStringContext (
            lib.concatMapStringsSep ":" toString (buildLibs ++ dlopenLibs ++ [ pkgs.stdenv.cc ])
          )
        )
      );

      # Vulkan drivers of the nixpkgs Mesa. A Mesa installed on the system is
      # never used inside the shell: it is built against another glibc and
      # LLVM.
      mesaIcdDir = "${pkgs.mesa}/share/vulkan/icd.d";
      lavapipeIcd = "${mesaIcdDir}/lvp_icd.x86_64.json";

      # Variables from outside the shell that would make the Vulkan loader,
      # the dynamic linker or CMake pick up the system's files. They are
      # dropped when the shell starts. (PKG_CONFIG_PATH is replaced by
      # `nix develop` itself.)
      outsideVariables = [
        "LD_LIBRARY_PATH"
        "VULKAN_SDK"
        "VK_SDK_PATH"
        "VK_ICD_FILENAMES"
        "VK_ADD_DRIVER_FILES"
        "VK_ADD_LAYER_PATH"
        "VK_ADD_IMPLICIT_LAYER_PATH"
        "VK_INSTANCE_LAYERS"
        "VK_LOADER_LAYERS_ENABLE"
        "VK_LOADER_LAYERS_DISABLE"
        "CMAKE_PREFIX_PATH"
      ];

      # tools/vm/guest.nix reads flake.lock itself and imports the same
      # nixpkgs, without the overlay above.
      vm = import ./tools/vm/guest.nix { inherit system; };

      # Both shells run the checks under the same conditions; they differ
      # only in the tools they carry.
      mkDevShell =
        {
          rust,
          extraPackages ? [ ],
        }:
        pkgs.mkShell {
          packages = [
            rust
          ]
          ++ (with pkgs; [
            pkg-config
            cargo-nextest
            # The Slang compiler. Build scripts compile the repository's
            # .slang files to SPIR-V with it (HIZUKURI_SLANGC below).
            shader-slang
            vulkan-tools # vulkaninfo, vkcube
            # tools/ci-watch.sh takes the backtraces of tests that got stuck,
            # and of processes that crashed, with it.
            gdb
            # The applications the tests can start
            # (HIZUKURI_TEST_REAL_CLIENTS).
            weston # weston-terminal
            wl-clipboard
            foot
          ])
          ++ extraPackages;

          buildInputs = buildLibs;

          # Put the dlopen()ed libraries on the RUNPATH of every binary
          # linked here.
          NIX_LDFLAGS = lib.concatMapStringsSep " " (p: "-rpath ${lib.getLib p}/lib") dlopenLibs;
          # Without this, `nix develop` also adds ./outputs/out/lib (the
          # would-be install prefix) to every RUNPATH.
          NIX_NO_SELF_RPATH = "1";

          # The loader sees lavapipe and nothing else, so the checks give
          # the same result on every machine. To use a hardware driver of
          # the nixpkgs Mesa, point this at HIZUKURI_ICD_DIRS inside the
          # shell.
          VK_DRIVER_FILES = lavapipeIcd;
          # Explicit layers come from nixpkgs only. Implicit layers are
          # searched in an empty directory: otherwise the loader finds the
          # ones installed on the system through XDG_DATA_DIRS and the home
          # directory, and tries to load their libraries.
          VK_LAYER_PATH = "${pkgs.vulkan-validation-layers}/share/vulkan/explicit_layer.d";
          VK_IMPLICIT_LAYER_PATH = "${pkgs.emptyDirectory}";

          # The slangc that builds use (prototype/slangc finds it here first,
          # then on PATH), so that every build uses the one flake.lock pins.
          HIZUKURI_SLANGC = "${pkgs.shader-slang}/bin/slangc";

          # The lavapipe manifest, for choosing it by hand:
          #   proto-compositor --vulkan-device cpu --vulkan-icd "$HIZUKURI_ICD_LAVAPIPE"
          HIZUKURI_ICD_LAVAPIPE = lavapipeIcd;
          # Where the tests look for the lavapipe manifest
          # (prototype/compositor/tests/common/icd.rs), ahead of the system
          # directories. Without it they would find a Mesa installed on the
          # system. The directory holds every driver of the nixpkgs Mesa.
          HIZUKURI_ICD_DIRS = mesaIcdDir;

          # Off NixOS there is no /run/opengl-driver, so GBM and EGL have
          # to be told where the nixpkgs Mesa is.
          GBM_BACKENDS_PATH = "${pkgs.mesa}/lib/gbm";
          __EGL_VENDOR_LIBRARY_FILENAMES = "${pkgs.mesa}/share/glvnd/egl_vendor.d/50_mesa.json";

          shellHook = ''
            unset ${lib.concatStringsSep " " outsideVariables}

            # Binaries built here link against /nix/store, binaries built
            # outside the shell against the system's libraries; see
            # linkHash above.
            if [ -z "''${CARGO_TARGET_DIR:-}" ]; then
              hizukuri_root="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
              export CARGO_TARGET_DIR="$hizukuri_root/target/nix-${linkHash}"
              unset hizukuri_root
            fi

            # llvmpipe generates code for the CPU it runs on. Fixing the
            # thread count narrows the differences between machines
            # (docs/dev/testing-and-ci.md). The vector width is left to
            # Mesa: each shader compile rewrites the process-wide width,
            # first to the CPU's own and then to LP_NATIVE_VECTOR_WIDTH,
            # without a lock, so any other value lets compiles running on
            # other threads build mismatched code.
            export LP_NUM_THREADS="''${LP_NUM_THREADS:-4}"
          '';
        };
    in
    {
      devShells.${system} = {
        # Everyday shell: adds editor support and more applications to try
        # the compositor with.
        default = mkDevShell {
          rust = rustToolchain [
            "rust-src"
            "rust-analyzer"
          ];
          extraPackages = with pkgs; [
            python3 # tools/real-app-check.sh
            alacritty
            wayland-utils # wayland-info
            wev
            xwayland
            xwayland-satellite
            xeyes
            xterm
            (qt6Packages.fcitx5-with-addons.override { addons = [ fcitx5-mozc ]; })
            drm_info
            nixfmt
            actionlint # checks .github/workflows
          ];
        };

        # What CI runs in: only what tools/ci.sh needs.
        ci = mkDevShell {
          rust = rustToolchain [ ];
        };

        # What the release report (a PDF of slides) is typeset with: Typst
        # and the fonts it is told to use, and nothing else. A shell of its
        # own, so that neither of the shells above fetches them. Typst is run
        # with --ignore-system-fonts, so the fonts named here are the only
        # ones it can find, on every machine.
        slides = pkgs.mkShellNoCC {
          packages = [ pkgs.typst ];
          TYPST_FONT_PATHS = lib.concatStringsSep ":" [
            "${pkgs.noto-fonts-cjk-sans-static}/share/fonts"
            "${pkgs.jetbrains-mono}/share/fonts"
          ];
        };
      };

      # The verification VM (tools/vm): the guest system and the tools that
      # run it. `tools/vm/vm` builds the same two derivations from guest.nix
      # directly; they are shown here so that `nix flake check` evaluates
      # them, and so that they can be built with `nix build`.
      packages.${system} = {
        vm-guest = vm.system;
        vm-host-tools = vm.hostTools;
      };

      formatter.${system} = pkgs.nixfmt;
    };
}
