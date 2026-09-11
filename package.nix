{ lib
, rustPlatform
}:

rustPlatform.buildRustPackage {
  pname = "brtt";
  version = "0.1.5";

  src = lib.cleanSourceWith {
    src = ./.;
    filter = path: type:
      let
        name = baseNameOf path;
      in
        name != "target"
        && name != ".direnv"
        && name != "result"
        && name != "result-bin";
  };

  cargoLock.lockFile = ./Cargo.lock;

  # Tests are run separately; avoid rebuilding test artifacts during packaging.
  doCheck = false;

  meta = {
    description = "A command-line RTT client";
    homepage = "https://github.com/michal4132/brtt";
    license = lib.licenses.mit;
    maintainers = [ ];
    mainProgram = "brtt";
    platforms = lib.platforms.unix;
  };
}
