# Amphi end to end in VMs: the NixOS module, nix-eval-jobs, nix build, and
# nix copy to a remote worker. GitHub and niks3 are fakes; the Rust tests
# cover their edge cases. It takes a few minutes; run it with
# `mise run test:vm`.
{
  amphi,
  lib,
  openssl,
  path,
  python3,
  runCommand,
  testers,
  writeText,
  writers,
}:

let
  repoConfig = ''
    branches = ["master"]
    issues = ["master"]

    [[jobsets]]
    name = "x86_64-linux"
    attr = "jobs"
  '';

  # A flake without inputs, since the VMs are offline. The sandbox's /bin/sh
  # is the only builder there is.
  flake = brokenScript: ''
    {
      outputs = { self }:
        let
          build = name: script: attrs: derivation ({
            inherit name;
            system = "x86_64-linux";
            builder = "/bin/sh";
            args = [ "-c" script ];
          } // attrs);
          lib = build "lib" "echo lib > $out" { };
        in {
          jobs = {
            app = build "app" "read line < ''${lib}; echo \"app uses $line\" > $out" { };
            remote = build "remote" "echo remote > $out" { requiredSystemFeatures = [ "remote-only" ]; };
            broken = build "broken" ${builtins.toJSON brokenScript} { };
          };
        };
    }
  '';

  first = "1111111111111111111111111111111111111111";
  fixed = "2222222222222222222222222222222222222222";

  commits = runCommand "amphi-vm-test-commits" { } ''
    mkdir -p $out/${first} $out/${fixed}
    cp ${builtins.toFile "flake.nix" (flake "echo failing on purpose; exit 1")} $out/${first}/flake.nix
    cp ${builtins.toFile "flake.nix" (flake "echo fixed > $out")} $out/${fixed}/flake.nix
    for commit in $out/*; do
      cp ${builtins.toFile "amphi.toml" repoConfig} $commit/amphi.toml
    done
  '';

  # A CA, so that the certificate itself need not be trusted as one.
  certificates = runCommand "amphi-vm-test-certificates" { nativeBuildInputs = [ openssl ]; } ''
    mkdir $out
    openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj /CN=fake-ca \
      -keyout ca.key -out $out/ca.pem
    openssl req -newkey rsa:2048 -nodes -subj /CN=api.github.com \
      -keyout $out/github.key -out github.csr
    printf 'subjectAltName=DNS:api.github.com\n' > extensions
    openssl x509 -req -in github.csr -CA $out/ca.pem -CAkey ca.key -CAcreateserial \
      -days 3650 -extfile extensions -out $out/github.pem
  '';

  fakeNiks3 = writers.writePython3Bin "niks3" { } (builtins.readFile ./vm-test/fake-niks3.py);

  sshKeys = import (path + "/nixos/tests/ssh-keys.nix") { inherit lib writeText; };

  workerSettings = name: {
    token-file = "/etc/amphi/worker-${name}-token";
    systems = [ "x86_64-linux" ];
    max-jobs = 2;
  };

  workerNode = {
    services.amphi.package = amphi;
    services.amphi.worker = {
      enable = true;
      niks3Package = fakeNiks3;
    };
    nix.settings.substituters = lib.mkForce [ ];
    environment.etc."amphi/worker-server-token".text = "token-server";
    environment.etc."amphi/worker-builder-token".text = "token-builder";
  };
in
testers.runNixOSTest {
  name = "amphi";

  nodes = {
    server =
      { pkgs, ... }:
      {
        imports = [
          ./module.nix
          workerNode
        ];
        virtualisation.memorySize = 2048;
        networking.hosts."127.0.0.1" = [ "api.github.com" ];
        security.pki.certificateFiles = [ "${certificates}/ca.pem" ];
        environment.etc."amphi/github-token".text = "github-token";

        services.amphi.server = {
          enable = true;
          settings = {
            listen = "0.0.0.0:8080";
            metrics-listen = "127.0.0.1:9100";
            public-url = "http://server:8080";
            repos = [ "owner/demo" ];
            github = {
              token-file = "/etc/amphi/github-token";
              poll-interval = 2;
            };
            workers = [
              {
                name = "server";
                token-file = "/etc/amphi/worker-server-token";
              }
              {
                name = "builder";
                token-file = "/etc/amphi/worker-builder-token";
                store-uri = "ssh-ng://root@builder?ssh-key=/var/lib/amphi/ssh-key";
              }
            ];
          };
        };
        services.amphi.worker.settings = workerSettings "server" // {
          server-url = "http://127.0.0.1:8080";
        };
        programs.ssh.extraConfig = ''
          Host builder
            StrictHostKeyChecking no
            UserKnownHostsFile /dev/null
        '';
        networking.firewall.allowedTCPPorts = [ 8080 ];

        systemd.services.fake-github = {
          wantedBy = [ "multi-user.target" ];
          before = [ "amphi-server.service" ];
          serviceConfig = {
            ExecStart = lib.escapeShellArgs [
              (lib.getExe python3)
              ./vm-test/fake-github.py
              commits
              "/var/lib/fake-github"
              "${certificates}/github.pem"
              "${certificates}/github.key"
            ];
            StateDirectory = "fake-github";
          };
        };
        environment.systemPackages = [
          pkgs.curl
          pkgs.jq
        ];
      };

    builder = {
      imports = [
        ./module.nix
        workerNode
      ];
      services.amphi.worker.settings = workerSettings "builder" // {
        server-url = "http://server:8080";
        features = [ "remote-only" ];
      };
      nix.settings.system-features = [ "remote-only" ];
      services.openssh.enable = true;
      users.users.root.openssh.authorizedKeys.keys = [ sshKeys.snakeOilPublicKey ];
    };
  };

  testScript = ''
    import json

    start_all()
    server.wait_for_unit("amphi-server.service")
    server.wait_for_unit("amphi-worker.service")
    builder.wait_for_unit("amphi-worker.service")
    builder.wait_for_unit("sshd.service")
    server.succeed("install -o amphi -m 400 ${sshKeys.snakeOilPrivateKey} /var/lib/amphi/ssh-key")

    def sql(query):
        return server.succeed(f"runuser -u amphi -- psql amphi -tAc \"{query}\"").strip()

    def push(sha):
        branches = json.dumps({"master": sha})
        server.succeed(f"echo '{branches}' > /var/lib/fake-github/branches.json")

    def wait_for_run(sha, state):
        server.wait_until_succeeds(
            f"[ \"$(runuser -u amphi -- psql amphi -tAc \"SELECT state FROM runs WHERE sha = '{sha}'\")\" = {state} ]",
            timeout=300,
        )

    def issues():
        return json.loads(server.succeed("cat /var/lib/fake-github/issues.json"))

    with subtest("a push is evaluated and built on both workers"):
        push("${first}")
        wait_for_run("${first}", "failed")
        assert sql("SELECT worker FROM builds WHERE drv_path LIKE '%-remote.drv'") == "builder"
        assert sql("SELECT state FROM builds WHERE drv_path LIKE '%-app.drv'") == "succeeded"
        assert "failing on purpose" in sql("SELECT log_tail FROM builds WHERE drv_path LIKE '%-broken.drv'")
        remote = sql("SELECT outputs->>'out' FROM builds WHERE drv_path LIKE '%-remote.drv'")
        builder.succeed(f"nix-store --check-validity {remote}")
        builder.succeed(f"grep -x {remote} /var/lib/amphi-worker/pushed")

    with subtest("the failure opens an issue and the roots are dropped"):
        server.wait_until_succeeds("[ -e /var/lib/fake-github/issues.json ]")
        [issue] = issues()
        assert issue["state"] == "open", issue
        assert "failing on purpose" in issue["body"], issue
        run = sql("SELECT id FROM runs WHERE sha = '${first}'")
        server.wait_until_succeeds(f"[ ! -e /var/lib/amphi/gcroots/{run} ]")

    with subtest("a fix closes the issue"):
        push("${fixed}")
        wait_for_run("${fixed}", "succeeded")
        server.wait_until_succeeds("jq -e '.[0].state == \"closed\"' /var/lib/fake-github/issues.json")
        assert sql("SELECT count(*) FROM builds WHERE drv_path LIKE '%-app.drv'") == "1"

    with subtest("metrics are served"):
        server.succeed("curl -sf http://127.0.0.1:9100/metrics | grep amphi_evaluation_seconds_count")
  '';
}
