{
  pkgs,
  module,
  agent,
}:

pkgs.testers.runNixOSTest {
  name = "nexigon-agent-service";
  nodes.machine = {
    imports = [ module ];
    environment.systemPackages = [ pkgs.curl ];
    services.nexigon-agent = {
      enable = true;
      package = agent;
      dataPath = "/var/lib/test-agent";
      provisioning = {
        enable = true;
        listenAddress = "127.0.0.1";
        port = 16947;
        deviceName = "nixos-test";
      };
    };
  };

  # Exercise provisioning, private configuration, and state retention across service restarts.
  testScript = ''
    import tomllib

    machine.start()
    machine.wait_for_unit("nexigon-agent.service")
    machine.wait_until_succeeds("curl --fail --silent http://127.0.0.1:16947/ | grep -q ready")
    assert machine.succeed("stat -c %a /run/nexigon/agent.toml").strip() == "600"
    assert machine.succeed("stat -c %a /var/lib/test-agent").strip() == "700"
    config = tomllib.loads(machine.succeed("cat /run/nexigon/agent.toml"))
    assert config["data-path"] == "/var/lib/test-agent"
    assert config["provisioning"]["device-name"] == "nixos-test"
    machine.succeed("touch /var/lib/test-agent/retained")
    machine.succeed("systemctl restart nexigon-agent.service")
    machine.wait_until_succeeds("curl --fail --silent http://127.0.0.1:16947/ | grep -q ready")
    machine.succeed("test -f /var/lib/test-agent/retained")
  '';
}
