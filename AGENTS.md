# AGENTS.md

Amphi is a Nix build coordinator for GitHub repositories. See README.md for its design.

## Layout

- `src/`, `migrations/`: the Rust crate, with the `amphi-server` and `amphi-worker` binaries.
- `web/`: the web UI, a Preact app built with Vite+ (`vp`).
- `nix/`: the packages, the NixOS module, and the VM test.
- `flake.nix`: `packages`, `overlays.default`, `nixosModules.default`, and `checks`.

## Tasks

Mise installs the toolchain and runs the tasks.

- `mise run check` runs the static checks and the unit tests; `mise run format` formats everything.
- `mise run test` also runs the server's tests on a temporary Postgres server.
- `mise run check:nix` checks Nix formatting and evaluates the flake.
- `mise run test:vm` runs the VM test of a deployment, which takes a few minutes.

## Nix packaging

- `amphi-web` fetches its dependencies with a fixed-output hash over `pnpm-lock.yaml`. After changing the lockfile, run `mise run nix:hash:update`, then `mise run nix:hash:check`. Register every new fixed-output hash in `nix/hash-targets.toml`.
- The package pins pnpm with `pnpm_<major>`, matching the pnpm major version in `mise.toml`: pnpm's store format, and with it the hash, changes between major versions. Bump both together, then update the hash.
- The NixOS module takes its packages from `pkgs.amphi` and `pkgs.amphi-web`, which `overlays.default` adds. Consumers apply the overlay on their own nixpkgs.
- CI pins the niks3 actions to exact versions, since a niks3 client must not be newer than the niks3 server.
