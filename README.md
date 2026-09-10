# ffmpeg-example
A simple example flake and crate for running `cargo-dyndrv`.

Build it with:
```shell
nix build github:obsidiansystems/cargo-dyndrv/ffmpeg-example
```

There should be a file named `ffmpeg_example` inside `result-ffmpeg-example`.

## Cross
It is also possible to cross-compile for aarch64-linux from x86_64-linux:
```shell
nix build github:obsidiansystems/cargo-dyndrv/ffmpeg-example#cross
```
The resulting binary can run in qemu-user.

## Requirements
This requires a version of Nix with support for `builder-rpc-v0`:
either git Nix from August 2026 or later or Nix 2.36 once it is released.

The build also requires the `ca-derivations` and `dynamic-derivations` experimental features.
