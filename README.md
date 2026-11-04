# sourcejump-packer

Packs Counter-Strike: Source maps for [SourceJump](https://sourcejump.net)'s replay viewer and zone editor. A map
is stripped down to what the viewer draws and compressed so browsers can unpack it themselves:

- sounds and model files the viewer never loads are taken out of the map's embedded files;
- the HDR lighting goes when the map has normal lighting too, and so does collision data (the viewer has no physics);
- the map's own textures are capped at 1024 pixels a side;
- what's left is compressed with brotli, which every browser decodes natively.

The result is usually a quarter to a fifth of the map's size. It's still a valid map file.

## sj-pack

`sj-pack` packs a whole folder of maps, several at once, for SourceJump staff to upload in bulk.

**Download** the zip from [Releases](../../releases). It holds `sj-pack.exe` (Windows), `sj-pack` (Linux) and a
short README. Nothing else needs installing.

- **Windows:** put `sj-pack.exe` in the folder with your `.bsp` or `.bsp.bz2` files and double-click it. Windows
  may warn that the program is unrecognised, because it isn't signed with a paid certificate: click
  *More info*, then *Run anyway*.
- **Linux:** `chmod +x sj-pack`, then `./sj-pack /path/to/maps` (or run it inside the folder). Run it from a terminal:
  double-clicked in a file manager it runs with no window, so its progress can't be seen or stopped.

Each map is written to `packed/<map>.viewer.br`. Stop it any time and run it again later: maps already packed are
skipped. `--jobs N` sets how many maps pack at once (default up to 4); use fewer if very large maps run the
computer out of memory.

### Checking a download

Release files are built by GitHub Actions from the tagged commit ([`release.yml`](.github/workflows/release.yml)),
never on someone's own computer. Each release lists the files' SHA-256 sums in `SHA256SUMS`, and GitHub signs a
record of the build:

```
gh attestation verify sj-pack.exe --repo sourcejump/sourcejump-packer
```

That shows the workflow run and the exact commit the file was built from. `sj-pack --version` prints the
version to compare.

### Building it yourself

With [rustup](https://rustup.rs) installed, `cargo build --release -p sj-pack` builds it for your own computer
into `target/release/`. The toolchain version is pinned in `rust-toolchain.toml`.

## Layout

- `crates/viewer-pack`: the packer as a library. SourceJump's website uses the same code, built for
  WebAssembly, to pack single maps in the browser, and its API uses `check` to accept only files this packer
  could have made.
- `crates/sj-pack`: the command-line tool.

The stripping rules are a Rust port of daf's `viewerpack.py` (checked byte-identical on real maps), plus the
texture size cap.

## License

MIT; see [LICENSE](LICENSE).
