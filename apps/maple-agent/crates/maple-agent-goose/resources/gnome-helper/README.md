# Vendored GNOME Shell helper

`metadata.json` and `extension.js` are copied verbatim from the Cua Driver
source revision that `crates/maple-agent/Cargo.toml` pins, at
`libs/cua-driver/wayland-helper/winrects@cua/`.

They are embedded in the binary so a user who installs only the executable can
still install the helper from Settings.

The driver and the extension negotiate an API version at run time, so these
files move together with the SDK pin. When the pin changes, re-copy both files
and update `UPSTREAM_SOURCE_REVISION` in `src/agent/gnome_helper.rs`.
