# sourcejump-packer
Public repo: the map packer for SourceJump's replay viewer (`crates/viewer-pack`, a library) and `sj-pack`, the
CLI staff run to pack folders of maps (`crates/sj-pack`). The private `sourcejump` repo depends on
`viewer-pack` by git revision for its API's upload check and its in-browser WebAssembly packer; after changing
the packer here, bump that revision there (its `api/crates/api/Cargo.toml` and `api/crates/viewer-pack-web/Cargo.toml`),
rebuild its `web/public/viewer-pack.wasm` with `just wasm`, and check that maps stored earlier still pass `check`.

- Check: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` (CI runs the same).
- Release: bump `version` in `crates/sj-pack/Cargo.toml`, commit, tag `v<version>`, push the tag;
  `.github/workflows/release.yml` builds and publishes. Never upload binaries built elsewhere.
- This repo is public: no private notes, server details or internal decisions in code, comments or commits.
- Same standards as `sourcejump`: one-line purpose comment per file, comments explain why, strict lints,
  `unsafe` forbidden, every new dependency justified.
