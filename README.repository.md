# Standalone Amaterasu audio encoder source

This crate builds the `amaterasu-audio-encode` native module. See `README.md`
for its PCM/Opus contract and bundled codec dependencies. `Cargo.toml` pins the
public `nonary-streams/amaterasu-sdk` Git identity and exact SDK commit;
`Cargo.lock` records the locally validated resolution. After dependencies and
CMake are available, run `cargo test --locked --offline`. The source began at
Amaterasu `a6b0db9e36a1a59328ab4b4785b27b210f8182b7` and was checked
against the latest isolated integration. The bundled Opus notice is retained
in `third-party/opus-COPYING`; binary distributors must include that notice.
No codec was installed or selected on a live host. Project-authored source is
MIT licensed; the bundled Opus notice remains separate.
