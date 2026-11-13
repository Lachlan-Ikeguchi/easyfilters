{
  description = "EasyFilters — Linux virtual camera with ONNX background filters (Rust)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # V4L2 is Linux-only.
      linuxSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs linuxSystems (system: f (import nixpkgs { inherit system; }));
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          libclangLib = pkgs.llvmPackages.libclang.lib;
          easyfilters = pkgs.rustPlatform.buildRustPackage {
            pname = "easyfilters";
            # Milestone 6: first packaged release (desktop file + icon,
            # NixOS autostart option).
            version = "0.2.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = with pkgs; [
              pkg-config
              clang
              makeWrapper
            ];
            buildInputs = with pkgs; [
              # opencv crate: found via opencv4.pc (pkg-config probe is boosted
              # automatically because PKG_CONFIG_PATH is set by the hook).
              opencv4
              # bindgen (v4l2r + opencv crates) needs libclang at build time;
              # the opencv crate's clang-runtime feature dlopens it, so both
              # vars must point at it.
              libclangLib
              # v4l2r binds linux/videodev2.h; in the sandbox the header lives
              # in linuxHeaders, not /usr/include/linux.
              linuxHeaders
              # libstdc++ for the opencv libs the binary links against.
              stdenv.cc.cc.lib
              # winit (iced, milestone 4): wayland/x11 backends link or dlopen
              # libwayland and libxkbcommon at build/runtime.
              wayland
              libxkbcommon
              # tray-item's ksni backend links libdbus (via libdbus-sys) at
              # build time.
              dbus
            ];

            LIBCLANG_PATH = "${libclangLib}/lib";
            V4L2R_VIDEODEV2_H_PATH = "${pkgs.linuxHeaders}/include/linux";
            preBuild = ''
              export LD_LIBRARY_PATH="${libclangLib}/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
            '';

            # No unit tests in the crate; checks.default below smoke-tests the
            # built binary instead.
            doCheck = false;

            postInstall = ''
              mkdir -p "$out/share/easyfilters"
              cp assets/selfie_segmentation_landscape.onnx \
                 assets/face_landmarks_478.onnx \
                 assets/blaze_face_short_range.onnx \
                 "$out/share/easyfilters/"
              # Milestone 6 packaging: app-menu entry + hicolor icon (the
              # same green rounded square the tray icon is generated from).
              mkdir -p "$out/share/applications" \
                       "$out/share/icons/hicolor/scalable/apps"
              cp packaging/easyfilters.desktop "$out/share/applications/"
              cp assets/easyfilters.svg "$out/share/icons/hicolor/scalable/apps/easyfilters.svg"
              wrapProgram "$out/bin/easyfilters" \
                --set-default EASYFILTERS_MODEL "$out/share/easyfilters/selfie_segmentation_landscape.onnx" \
                --set-default EASYFILTERS_FACE_MODEL "$out/share/easyfilters/face_landmarks_478.onnx" \
                --set-default EASYFILTERS_FACE_DETECTOR "$out/share/easyfilters/blaze_face_short_range.onnx" \
                --prefix LD_LIBRARY_PATH : "${
                  pkgs.lib.makeLibraryPath [
                    pkgs.stdenv.cc.cc.lib
                    pkgs.wayland
                    pkgs.libxkbcommon
                    pkgs.dbus.lib
                  ]
                }"
            '';

            meta = with pkgs.lib; {
              description = "Virtual camera with background filters (v4l2loopback + ONNX segmentation)";
              mainProgram = "easyfilters";
              platforms = platforms.linux;
            };
          };
        in
        {
          default = easyfilters;
          easyfilters = easyfilters;
        }
      );

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${self.packages.${pkgs.stdenv.hostPlatform.system}.default}/bin/easyfilters";
        };
      });

      checks = forAllSystems (pkgs: {
        # Builds the package (shared store path) and smoke-tests the binary.
        # Device-dependent paths (--list/--self-test) need /dev/video*, which
        # the sandbox does not have.
        default = pkgs.runCommand "easyfilters-smoke" { } ''
          pkg=${self.packages.${pkgs.stdenv.hostPlatform.system}.default}
          $pkg/bin/easyfilters --help > "$out"
          # Milestone 6: the packaged desktop file + icon must be present.
          test -f "$pkg/share/applications/easyfilters.desktop"
          test -f "$pkg/share/icons/hicolor/scalable/apps/easyfilters.svg"
          grep -q "Icon=easyfilters" "$pkg/share/applications/easyfilters.desktop"
        '';
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            pkg-config
            clang
            opencv4
            llvmPackages.libclang.lib
            linuxHeaders
            # runtime/debug tooling used by the repo scripts and plans
            v4l-utils
            ffmpeg
            (python3.withPackages (ps: [ ps.opencv4 ]))
            # winit (iced GUI) dlopens these at runtime; dbus dev files let
            # libdbus-sys (tray-item/ksni) build via pkg-config
            wayland
            libxkbcommon
            dbus
          ];
          # Same env the package build uses, so plain `cargo build` inside
          # `nix develop` works without manual OPENCV_*/LIBCLANG_PATH exports.
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          V4L2R_VIDEODEV2_H_PATH = "${pkgs.linuxHeaders}/include/linux";
          shellHook = ''
            export LD_LIBRARY_PATH="${pkgs.llvmPackages.libclang.lib}/lib:${pkgs.wayland}/lib:${pkgs.libxkbcommon}/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
          '';
        };
      });

      # Declarative v4l2loopback setup for NixOS machines. On other distros,
      # scripts/setup-virtual-camera.sh remains the way to load the module.
      nixosModules.default =
        { config, lib, pkgs, ... }:
        let
          cfg = config.easyfilters;
        in
        {
          options.easyfilters = {
            enable = lib.mkEnableOption "EasyFilters virtual camera (v4l2loopback) setup";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              description = "The easyfilters package to install system-wide.";
            };
            cardLabel = lib.mkOption {
              type = lib.types.str;
              default = "EasyFilters Virtual Camera";
              description = "card_label of the v4l2loopback device (shown in app camera pickers).";
            };
            exclusiveCaps = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "exclusive_caps=1 hides the device from PipeWire clients; false exposes it to both PipeWire and V4L2 apps.";
            };
            autostart = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Start easyfilters at login as a systemd user service (window hidden, tray only) via --start-minimized. The user still needs the \"video\" group for /dev/video*.";
            };
          };
          config = lib.mkIf cfg.enable {
            boot.extraModulePackages = [ config.boot.kernelPackages.v4l2loopback ];
            boot.extraModprobeConfig = ''
              options v4l2loopback exclusive_caps=${
                if cfg.exclusiveCaps then "1" else "0"
              } card_label="${cfg.cardLabel}" video_nr=-1
            '';
            environment.systemPackages = [ cfg.package ];
            # /dev/video* nodes are accessible to the "video" group by default
            # udev rules; add your user to it if not already.
            # Milestone 6: launch at login as a user service (graphical
            # session), running in the background with the window hidden.
            systemd.user.services.easyfilters = lib.mkIf cfg.autostart {
              description = "EasyFilters virtual camera (background, tray only)";
              wantedBy = [ "graphical-session.target" ];
              after = [ "graphical-session.target" ];
              serviceConfig = {
                ExecStart = "${cfg.package}/bin/easyfilters --start-minimized";
                Restart = "on-failure";
                RestartSec = 5;
              };
            };
          };
        };
    };
}
