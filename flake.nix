{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    nixpkgs,
    crane,
    fenix,
    ...
  }: let
    systems = ["x86_64-linux"];
    perSystem = f:
      nixpkgs.lib.foldAttrs nixpkgs.lib.mergeAttrs {}
      (map (s: nixpkgs.lib.mapAttrs (_: v: {${s} = v;}) (f s)) systems);
  in
    perSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [fenix.overlays.default];
      };

      craneLib =
        (crane.mkLib pkgs).overrideToolchain
        (pkgs.fenix.complete.withComponents ["cargo" "clippy" "rustc" "rustfmt" "rust-src"]);

      src = pkgs.lib.fileset.toSource {
        root = ./.;
        fileset = craneLib.fileset.commonCargoSources ./.;
      };

      args = {
        inherit src;
        strictDeps = true;
        # nativeBuildInputs = with pkgs; [];
        # buildInputs = with pkgs; [ ];
      };

      cargoArtifacts = craneLib.buildDepsOnly args;
      cargoClippyExtraArgs = "--all-targets -- --deny warnings";

      package = craneLib.buildPackage (args
        // {
          inherit cargoArtifacts;
          meta.mainProgram = "bld";
        });
    in {
      devShells.default = craneLib.devShell {
        packages = with pkgs; [taplo rust-analyzer];
      };

      checks = {
        inherit package;

        fmt = craneLib.cargoFmt (args // {inherit cargoArtifacts;} // {cargoExtraArgs = "--all";});
        fmt-toml = craneLib.taploFmt {src = pkgs.lib.sources.sourceFilesBySuffices src [".toml"];};
        test = craneLib.cargoTest (args // {inherit cargoArtifacts;});
        clippy = craneLib.cargoClippy (args // {inherit cargoArtifacts cargoClippyExtraArgs;});
      };

      packages.default = package;
    });
}
