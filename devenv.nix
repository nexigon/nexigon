{
  pkgs,
  sidex,
  ...
}:
{
  packages = with pkgs; [
    cargo-deny
    taplo
    sidex.packages.${pkgs.stdenv.hostPlatform.system}.default
  ];

  languages.rust = {
    enable = true;
    channel = "nightly";
    version = "latest";
    mold.enable = true;
  };

  git-hooks.hooks = {
    nixfmt.enable = true;
    rustfmt.enable = true;
  };
}
