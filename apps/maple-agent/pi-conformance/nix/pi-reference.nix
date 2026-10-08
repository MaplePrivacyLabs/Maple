{
  lib,
  stdenv,
  nodejs_22,
  importNpmLock,
  fetchurl,
  git,
  python3,
  source,
  pin,
  name,
  script,
}:
let
  catalogPin = lib.importJSON (source + "/nix/model-catalog.json");
  modelCatalog = fetchurl {
    name = "pi-model-catalog.json";
    url = "https://pi.dev/api/models/revisions/${catalogPin.revision}?types=chat,image,classifier";
    sha256 = lib.removePrefix "sha256-" catalogPin.revision;
  };
in
assert catalogPin.revision == pin.catalogRevision;
assert builtins.hashFile "sha256" (source + "/package-lock.json") == pin.packageLockSha256;
stdenv.mkDerivation {
  pname = "pi-reference-${name}";
  version = lib.removePrefix "v" pin.tag;
  src = source;

  npmDeps = importNpmLock { npmRoot = source; };
  npmRebuildFlags = [ "--ignore-scripts" ];
  nativeBuildInputs = [
    nodejs_22
    importNpmLock.npmConfigHook
    git
    python3
  ];

  env = {
    TZ = "UTC";
    LANG = "C";
    LC_ALL = "C";
    PI_OFFLINE = "1";
    PI_NO_LOCAL_LLM = "1";
    AWS_EC2_METADATA_DISABLED = "true";
    GIT_CONFIG_NOSYSTEM = "1";
    GIT_CONFIG_GLOBAL = "/dev/null";
  };

  buildPhase = ''
    runHook preBuild
    export HOME="$TMPDIR/home"
    mkdir -p "$HOME" "$out"
    node packages/ai/scripts/hydrate-model-catalog.ts ${modelCatalog}
    node packages/ai/scripts/check-model-data.ts
    export PI_WORKSPACE_ROOT="$PWD"
    export PI_VITEST_CLI="$PWD/node_modules/vitest/dist/cli.js"
    node --version > "$out/node-version.txt"
    wc -c < ${modelCatalog} > "$out/catalog-bytes.txt"
    ${script}
    runHook postBuild
  '';

  dontInstall = true;
  dontFixup = true;
}
