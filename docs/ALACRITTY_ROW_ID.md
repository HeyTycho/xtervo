# Vendored alacritty_terminal RowId patch

`vendor/alacritty_terminal` is upstream `alacritty_terminal` 0.26.1 with one focused change:

- every `Row` has a stable `u64` identity;
- `Row::reset` assigns a new identity (recycled storage is a new logical line);
- `Storage::swap` uses the safe row swap so identity moves with the row;
- `Grid::row_id(Line)` and `Grid::find_row_id(u64)` expose identity;
- split reflow keeps the original id on the first segment; merged/evicted rows lose it and
  consumers must drop the placement explicitly.

The upstream source is Apache-2.0; the license file is included. The patch is applied through
`.cargo/config.toml`; `Cargo.lock` pins `alacritty_terminal 0.26.1`.

This is intentionally not a public alacritty fork dependency: the patch is vendored and
reproducible inside this repository.
