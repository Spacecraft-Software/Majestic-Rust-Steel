# SPDX-FileCopyrightText: 2026 Mohamed Hammad <Mohamed.Hammad@SpacecraftSoftware.org>
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Dev shell for running Nova's GPU front end (`mj-nova`) on NixOS.
#
# winit and wgpu `dlopen` their runtime libraries — libwayland-client + libxkbcommon (the Wayland
# window + keymap) and libvulkan (the GPU loader) — at startup, by soname. NixOS does not put those
# on the default loader path, so a bare `cargo run -p nova --features gpu --bin mj-nova` fails with
# `WaylandError(Connection(NoWaylandLib))`. This shell adds the (64-bit) libraries to
# `LD_LIBRARY_PATH`; the GPU driver ICDs are picked up from `/run/opengl-driver` as usual.
#
#   nix-shell crates/nova/shell.nix --run 'cargo run -p nova --features gpu --bin mj-nova'
#
# (see docs/nova.md — "Running Nova on NixOS").
{
  pkgs ? import <nixpkgs> { },
}:
pkgs.mkShell {
  LD_LIBRARY_PATH = "${
    pkgs.lib.makeLibraryPath [
      pkgs.wayland # libwayland-client / -cursor / -egl (winit Wayland backend)
      pkgs.libxkbcommon # libxkbcommon (winit keymap)
      pkgs.vulkan-loader # libvulkan (wgpu's GPU loader)
      pkgs.libGL # libEGL / libGL (the GL fallback path)
    ]
  }:/run/opengl-driver/lib";
}
