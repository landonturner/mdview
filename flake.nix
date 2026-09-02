{
  description = "A less-style terminal pager that renders markdown readably";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
        mdview = pkgs.rustPlatform.buildRustPackage {
          pname = manifest.name;
          version = manifest.version;
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;

          # onig_sys builds oniguruma from source via cc and needs libclang
          # for bindgen; pointing it at the system oniguruma avoids both.
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.oniguruma ];
          RUSTONIG_SYSTEM_LIBONIG = true;

          meta = with pkgs.lib; {
            description = manifest.description;
            homepage = manifest.repository;
            license = licenses.mit;
            mainProgram = "mdview";
          };
        };
      in
      {
        packages = {
          default = mdview;
          inherit mdview;
        };

        apps.default = flake-utils.lib.mkApp { drv = mdview; };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ mdview ];
          packages = [ pkgs.cargo pkgs.rustc pkgs.rust-analyzer pkgs.clippy ];
          RUSTONIG_SYSTEM_LIBONIG = true;
        };
      });
}
