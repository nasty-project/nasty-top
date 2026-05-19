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
            useFetchCargoVendor = true;
            cargoHash = "sha256-d1gVWeQMLx03/qE62qY4Yk+Xqa/bWfdpIg8sjV4XX50=";
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
