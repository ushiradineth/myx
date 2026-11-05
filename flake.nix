{
  description = "Myx - a terminal Spotify player";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [ "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          inherit (pkgs) lib;
        in
        rec {
          myx = pkgs.rustPlatform.buildRustPackage {
            pname = "myx";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;

            src = lib.cleanSourceWith {
              src = ./.;
              filter = path: type:
                let
                  relative = lib.removePrefix (toString ./. + "/") (toString path);
                  parts = lib.splitString "/" relative;
                  top = builtins.head parts;
                in
                lib.cleanSourceFilter path type
                && (toString path == toString ./. || builtins.elem top [
                  "Cargo.toml" "Cargo.lock" "LICENSE" "NOTICE" "README.md"
                  "src" "crates" "examples" "tests" "nix" "flake.lock" "packaging"
                ])
                && !lib.any
                  (component: builtins.elem component [ ".agents" ".runtime" ".cache" "__pycache__" "target" "tools" "result" "assets" "credentials" "api-key" ])
                  parts;
            };
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              pkgs.pkg-config
            ];

            buildInputs =
              lib.optionals pkgs.stdenv.hostPlatform.isLinux [
                pkgs.alsa-lib
                pkgs.openssl
              ]
              ++ lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
                pkgs.libiconv
              ];

            strictDeps = true;

            meta = {
              description = "A lean, beautiful terminal Spotify player";
              homepage = "https://github.com/HaseebKhalid1507/Myx";
              license = lib.licenses.mit;
              mainProgram = "myx";
              platforms = supportedSystems;
            };
          };

          default = myx;
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.myx}/bin/myx";
        };
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.myx ];
            packages = with pkgs; [
              cargo
              clippy
              rust-analyzer
              rustc
              rustfmt
              python3
            ];
          };
        }
      );
    };
}
