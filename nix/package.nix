{
  aws-lc,
  cacert,
  callPackage,
  lib,
  pkg-config,
  postgresql,
  postgresqlTestHook,
  rustPlatform,
}:

let
  root = ../.;
in
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "amphi";
  version = "0.1.0";

  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (root + "/Cargo.toml")
      (root + "/Cargo.lock")
      (root + "/migrations")
      (root + "/src")
    ];
  };
  cargoLock.lockFile = root + "/Cargo.lock";

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ aws-lc ];

  # The server's tests need Postgres, and create a database each. Its HTTP
  # clients load the system's CA certificates.
  nativeCheckInputs = [
    cacert
    postgresql
    postgresqlTestHook
  ];
  postgresqlTestUserOptions = "LOGIN CREATEDB";
  postgresqlTestSetupPost = ''
    export DATABASE_URL="postgres:///$PGDATABASE?host=$PGHOST&user=$PGUSER"
  '';
  checkFlags = [ "--include-ignored" ];

  passthru.tests.vm = callPackage ./vm-test.nix { amphi = finalAttrs.finalPackage; };

  meta = {
    description = "Nix build coordinator for GitHub repositories";
    mainProgram = "amphi-server";
  };
})
