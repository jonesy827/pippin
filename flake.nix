{
  description = "Linux client for macOS high-performance Screen Sharing";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "pippin";
            # Version follows Cargo.toml so the flake never drifts on release bumps.
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;

            src = ./.;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            nativeBuildInputs = with pkgs; [
              pkg-config
              clang # bindgen needs clang to parse the FFmpeg headers
              makeWrapper
            ];

            buildInputs = with pkgs; [
              ffmpeg # libavcodec + libavutil
              alsa-lib # cpal audio
              libxkbcommon
            ];

            env.LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";

            # Libraries loaded at runtime via dlopen (wgpu/winit/wl-clipboard-rs).
            postInstall = ''
              install -Dm644 assets/pippin.desktop -t $out/share/applications
              install -Dm644 assets/pippin.svg -t $out/share/icons/hicolor/scalable/apps
              # Optional audio: pippin dlopens libfdk-aac.so; it's unfree in nixpkgs,
              # so it's left out by default. To enable audio, uncomment and set
              # NIXPKGS_ALLOW_UNFREE=1 (or allowUnfree = true in your config).
              wrapProgram $out/bin/pippin \
                --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath (with pkgs; [
                  vulkan-loader
                  libGL
                  wayland
                  libxkbcommon
                  # libfdk-aac
                ])}
            '';

            meta = with pkgs.lib; {
              description = "Linux client for macOS high-performance Screen Sharing";
              homepage = "https://github.com/jonesy827/pippin";
              license = licenses.agpl3Plus;
              mainProgram = "pippin";
              platforms = platforms.linux;
            };
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              rust-analyzer

              pkg-config
              clang
              ffmpeg
              alsa-lib
              libxkbcommon
            ];

            env.LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";

            # Let wgpu/winit find GPU and windowing libraries in dev builds.
            env.LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (
              with pkgs;
              [
                vulkan-loader
                libGL
                wayland
                libxkbcommon
                # libfdk-aac # unfree; enables audio (see package comment above)
              ]
            );
          };
        }
      );
    };
}
