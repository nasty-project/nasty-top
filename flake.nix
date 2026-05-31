{
  description = "nasty-top — a top-like TUI for bcachefs filesystems";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      forAllSystems = nixpkgs.lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "nasty-top";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            src = ./.;
            # Vendor deps via Cargo.lock instead of carrying a separate
            # cargoHash. cargoHash hashes the full `cargo vendor` tarball,
            # so any `cargo update` shifts it and downstream packagers
            # (#16, nasty's nasty.nix #362) discover this only when their
            # build breaks. cargoLock.lockFile has Nix synthesize one
            # fetchurl per crate keyed on the SHA Cargo itself already
            # wrote into Cargo.lock — zero hash to maintain across
            # releases, no drift possible.
            cargoLock.lockFile = ./Cargo.lock;
            meta = {
              description = "A top-like TUI for bcachefs filesystems";
              homepage = "https://github.com/nasty-project/nasty-top";
              license = pkgs.lib.licenses.gpl3Only;
              mainProgram = "nasty-top";
            };
          };
        }
      );
    };
}
