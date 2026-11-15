{
  lib,
  stdenvNoCC,
  nodejs,
  pnpm_12,
  pnpmConfigHook,
  pnpmBuildHook,
  fetchPnpmDeps,
}:

let
  # pnpm's store format, and with it the hash of the dependencies, changes
  # between major versions. Keep it on the major version mise installs.
  pnpm = pnpm_12;
  root = ../.;
  web = root + "/web";
  workspace = [
    (root + "/package.json")
    (root + "/pnpm-lock.yaml")
    (root + "/pnpm-workspace.yaml")
    (web + "/package.json")
  ];
in
stdenvNoCC.mkDerivation (finalAttrs: {
  pname = "amphi-web";
  version = "0.1.0";

  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions (
      workspace
      ++ [
        (root + "/tsconfig.json")
        (web + "/index.html")
        (web + "/src")
        (web + "/tsconfig.json")
        (web + "/vite.config.ts")
      ]
    );
  };

  nativeBuildInputs = [
    nodejs
    pnpm
    pnpmConfigHook
    pnpmBuildHook
  ];

  pnpmWorkspaces = [ "amphi-web" ];
  pnpmDeps = fetchPnpmDeps {
    inherit (finalAttrs) pname pnpmWorkspaces;
    src = lib.fileset.toSource {
      inherit root;
      fileset = lib.fileset.unions workspace;
    };
    inherit pnpm;
    fetcherVersion = 5;
    hash = "sha256-Ib610/BilR+j4N+JjA2TA/mLgizsKS1PZjXhpZiMLrI=";
  };

  installPhase = ''
    runHook preInstall
    cp -r web/dist "$out"
    runHook postInstall
  '';

  meta.description = "Web UI of Amphi";
})
