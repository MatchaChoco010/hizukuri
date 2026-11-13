# The verification VM, as Nix sees it. `tools/vm/vm` builds the attributes of
# this file; nothing here is installed on the machine that runs it.
#
#   system     the guest: a NixOS system. It is not installed on a disk. QEMU
#              boots its kernel and initrd directly, the root file system is a
#              tmpfs, and /nix/store is the store of the machine that runs
#              QEMU, shared read-only. Every start is a fresh guest.
#   hostTools  what `tools/vm/vm` runs outside the guest: QEMU, ssh, rsync,
#              python; and where that QEMU finds its Mesa.
#
# The flake shows both as packages (vm-guest, vm-host-tools), so that
# `nix flake check` evaluates them.
#
# Both come from the nixpkgs commit that flake.lock pins, the one the
# development shell is made from. That matters in two ways: binaries built in
# the development shell find their libraries in the guest (same store, same
# paths), and the guest's kernel, Mesa and applications only change when
# flake.lock does.
{
  system ? "x86_64-linux",
}:
let
  lock = builtins.fromJSON (builtins.readFile ../../flake.lock);
  pin = lock.nodes.${lock.nodes.${lock.root}.inputs.nixpkgs}.locked;
  nixpkgs = builtins.fetchTarball {
    url = "https://github.com/${pin.owner}/${pin.repo}/archive/${pin.rev}.tar.gz";
    sha256 = pin.narHash;
  };
  pkgs = import nixpkgs {
    inherit system;
    config = { };
    overlays = [ ];
  };
  inherit (pkgs) lib;

  user = "hizukuri";

  # The helper scripts of tools/vm/guest, on the guest's PATH. They are
  # copied to the store as a path of their own, so that the guest is the same
  # derivation whether this file is built directly or through the flake, and
  # only changes when the scripts do.
  guestScripts = builtins.path {
    path = ./guest;
    name = "hizukuri-vm-guest-scripts";
  };
  guestTools = pkgs.runCommand "hizukuri-vm-guest-tools" { } ''
    mkdir -p $out/bin
    cp ${guestScripts}/* $out/bin/
    chmod 0755 $out/bin/*
  '';

  # The input method the IME checks use: fcitx5 with the Anthy engine for
  # Japanese, nothing else (no Qt, no GTK modules, no configuration tool).
  # The wrapper tells fcitx5 where the engine's addon is, as nixpkgs'
  # fcitx5-with-addons does.
  inputMethod = pkgs.symlinkJoin {
    name = "hizukuri-vm-fcitx5";
    paths = [
      pkgs.fcitx5
      pkgs.fcitx5-anthy
    ];
    nativeBuildInputs = [ pkgs.makeBinaryWrapper ];
    postBuild = ''
      wrapProgram $out/bin/fcitx5 \
        --prefix FCITX_ADDON_DIRS : $out/lib/fcitx5 \
        --suffix XDG_DATA_DIRS : $out/share
    '';
  };

  guest =
    { modulesPath, ... }:
    {
      imports = [ "${modulesPath}/profiles/minimal.nix" ];

      system.stateVersion = lib.trivial.release;
      networking.hostName = "hizukuri-vm";

      # Booted by QEMU with -kernel and -initrd: there is no boot loader and
      # no disk.
      boot.loader.grub.enable = false;
      boot.kernelPackages = pkgs.linuxPackages_latest;
      boot.initrd.kernelModules = [
        "virtio_pci"
        "9p"
        "9pnet_virtio"
      ];
      boot.kernelModules = [ "qemu_fw_cfg" ];
      # Nothing but the compositor under test writes to the console's screen.
      boot.consoleLogLevel = 3;
      boot.kernel.sysctl."kernel.printk" = "3 4 1 3";

      fileSystems."/" = {
        device = "none";
        fsType = "tmpfs";
        options = [
          "mode=0755"
          "size=75%"
        ];
      };
      fileSystems."/nix/store" = {
        device = "nix-store";
        fsType = "9p";
        neededForBoot = true;
        options = [
          "trans=virtio"
          "version=9p2000.L"
          "ro"
          "cache=loose"
          "msize=1048576"
        ];
      };

      # The store is shared read-only: the guest never runs Nix.
      nix.enable = false;
      documentation.enable = false;
      hardware.enableRedistributableFirmware = false;

      networking.useDHCP = true;
      networking.firewall.enable = false;

      # SSH is the way in. The only key that is accepted is the one
      # `tools/vm/vm` made for this VM and hands over through fw_cfg.
      services.openssh = {
        enable = true;
        settings = {
          PasswordAuthentication = false;
          KbdInteractiveAuthentication = false;
          PermitRootLogin = "no";
        };
        hostKeys = [
          {
            type = "ed25519";
            path = "/etc/ssh/ssh_host_ed25519_key";
          }
        ];
      };
      systemd.services.hizukuri-authorized-key = {
        description = "Accept the SSH key of tools/vm/vm";
        wantedBy = [ "multi-user.target" ];
        before = [ "sshd.service" ];
        unitConfig.ConditionPathExists = "/sys/firmware/qemu_fw_cfg/by_name/opt/hizukuri/authorized_keys/raw";
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${pkgs.coreutils}/bin/install -D -m 0644 /sys/firmware/qemu_fw_cfg/by_name/opt/hizukuri/authorized_keys/raw /etc/ssh/authorized_keys.d/${user}";
        };
      };

      # The one account. The password is for logging in on the console of a
      # VM that is shown in a window; the VM is reachable from the machine
      # that runs it only.
      users.mutableUsers = false;
      users.users.${user} = {
        isNormalUser = true;
        uid = 1000;
        password = "hizukuri";
        extraGroups = [
          "wheel"
          "video"
          "input"
          "render"
          "seat"
          # lavapipe offers dma-buf modifiers only when it can open
          # /dev/udmabuf, which belongs to this group.
          "kvm"
        ];
      };
      security.sudo.wheelNeedsPassword = false;
      # logind asks polkit whether a session may switch the virtual terminal;
      # without polkit only root may.
      security.polkit.enable = true;

      # seatd is there for `tty-run --seatd`; logind is the default.
      services.seatd.enable = true;
      systemd.services.seatd.wantedBy = lib.mkForce [ ];

      # Mesa. The guest renders on the CPU: the Vulkan loader is pointed at
      # lavapipe only; the drivers for real GPUs have nothing to find here.
      hardware.graphics.enable = true;
      environment.sessionVariables.VK_DRIVER_FILES =
        "/run/opengl-driver/share/vulkan/icd.d/lvp_icd.x86_64.json";

      # Japanese text: what the input method composes, in the terminal and
      # in the candidate window.
      fonts.packages = [
        pkgs.dejavu_fonts
        pkgs.ipaexfont
      ];
      fonts.fontconfig.defaultFonts.monospace = [ "DejaVu Sans Mono" ];

      environment.systemPackages = [
        guestTools
        # For uinput-touchpad.py and x11-test-client.py.
        (pkgs.python3.withPackages (python: [
          python.libevdev
          python.python-xlib
        ]))
      ]
      ++ (with pkgs; [
        # Applications the checks open.
        foot
        weston # weston-terminal
        alacritty
        vulkan-tools # vkcube, vulkaninfo
        wl-clipboard
        # The input method, and the session bus it wants.
        inputMethod
        dbus
        # X11 applications, through xwayland-satellite (which brings Xwayland).
        xwayland-satellite
        xterm
        xclip
        xdotool
        xprop
        xwininfo
        # Clients that draw their own cursor show none without a theme.
        adwaita-icon-theme
        # Looking at devices and input.
        drm_info
        libinput
        evtest
        wayland-utils
        wev
        # Tools the helper scripts use.
        gdb
        strace
        rsync
        kbd
        vim
        vulkan-validation-layers
      ]);
      environment.variables.XCURSOR_PATH = [ "/run/current-system/sw/share/icons" ];
    };

  nixos = import "${nixpkgs}/nixos/lib/eval-config.nix" {
    inherit system;
    modules = [ guest ];
  };
in
{
  system = nixos.config.system.build.toplevel;

  hostTools = pkgs.buildEnv {
    name = "hizukuri-vm-host-tools";
    paths = with pkgs; [
      qemu_kvm
      openssh
      rsync
      python3
    ];
  };

  inherit pkgs;
}
