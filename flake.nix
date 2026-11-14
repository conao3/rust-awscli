{
  description = "A fast, output-compatible Rust reimplementation of the AWS CLI v2";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    treefmt-nix.url = "github:numtide/treefmt-nix";
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-darwin"
      ];

      imports = [ inputs.treefmt-nix.flakeModule ];

      perSystem =
        { pkgs, lib, ... }:
        let
          awscliReferenceSrc = pkgs.fetchFromGitHub {
            owner = "aws";
            repo = "aws-cli";
            rev = "2.40.2";
            hash = "sha256-h/kqpKdgPuEoB0pVyGiWeyD9w6NrQqelXuyJkl0QTvo=";
          };
          awscliReference = pkgs.stdenv.mkDerivation rec {
            pname = "awscli-reference";
            version = "2.40.2";
            src = pkgs.fetchurl {
              url = "https://awscli.amazonaws.com/awscli-exe-linux-x86_64-${version}.zip";
              hash = "sha256-qawNY16X+rZeI0rhUZ+mvSsC1yEgv5Zaj/bq92EfGDM=";
            };
            nativeBuildInputs = [ pkgs.unzip ];
            unpackPhase = "unzip -q $src";
            installPhase = ''
              mkdir -p $out/lib $out/bin
              cp -r aws/dist $out/lib/aws-cli
              cat > $out/bin/aws <<SH
              #!${pkgs.runtimeShell}
              export LD_LIBRARY_PATH=$out/lib/aws-cli:${
                lib.makeLibraryPath [
                  pkgs.zlib
                  pkgs.stdenv.cc.cc.lib
                ]
              }
              exec ${pkgs.stdenv.cc.libc}/lib/ld-linux-x86-64.so.2 $out/lib/aws-cli/aws "\$@"
              SH
              chmod +x $out/bin/aws
            '';
            meta.platforms = [ "x86_64-linux" ];
          };
          awsr = pkgs.rustPlatform.buildRustPackage {
            pname = "awsr";
            version = (lib.importTOML ./Cargo.toml).package.version;
            src = lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;
            doCheck = false;
            nativeBuildInputs = [ pkgs.groff ];
            preBuild = ''
              cargo build --release --frozen --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} -p awsr-helpgen
              HOME=$TMPDIR target/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/release/awsr-helpgen $TMPDIR/help-pages.bin
              export AWSR_HELP_PAGES=$TMPDIR/help-pages.bin
            '';
            postInstall = ''
              rm $out/bin/awsr-helpgen
            '';
            env = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
              RUSTFLAGS = "-C target-feature=+crt-static -L native=${pkgs.glibc.static}/lib";
            };
            meta = {
              description = "A fast, output-compatible Rust reimplementation of the AWS CLI v2";
              homepage = "https://github.com/conao3/rust-awscli";
              license = lib.licenses.asl20;
              mainProgram = "awsr";
              platforms = lib.platforms.unix;
            };
          };
        in
        {
          packages = {
            default = awsr;
            awsr-without-help-pages = awsr.overrideAttrs {
              preBuild = "";
              postInstall = "";
            };
          }
          // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux { awscli-reference = awscliReference; };

          devShells.default = pkgs.mkShell {
            packages =
              with pkgs;
              [
                cargo
                clippy
                gnumake
                groff
                rust-analyzer
                rustc
                rustfmt
                zstd
                jq
                hyperfine
                (python3.withPackages (
                  ps: with ps; [
                    awscrt
                    colorama
                    cryptography
                    distro
                    docutils
                    jmespath
                    prompt-toolkit
                    python-dateutil
                    ruamel-yaml
                    urllib3
                  ]
                ))
              ]
              ++ lib.optional stdenv.hostPlatform.isLinux awscliReference;
            AWSR_REFERENCE_SRC = "${awscliReferenceSrc}";
            AWSR_REFERENCE_AWS = lib.optionalString pkgs.stdenv.hostPlatform.isLinux "${awscliReference}/bin/aws";
          };

          treefmt.programs = {
            nixfmt.enable = true;
            rustfmt.enable = true;
          };
        };
    };
}
