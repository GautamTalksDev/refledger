# Calibration harness (not shipped)

Standalone Rust binary. **Not** a Cargo workspace member — see the empty
`[workspace]` table in `Cargo.toml`.

Builds with:

```bash
cargo build --manifest-path tools/calibrate/Cargo.toml --release
```

Requires `REFLEDGER_CALIBRATE_TOKEN`. Writes `docs/CALIBRATION.md` and raw CSVs
under `tools/calibrate/data/`. Honour guest policy: stop on first 429.
