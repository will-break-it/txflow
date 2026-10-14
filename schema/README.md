# Spec schema

`v<N>/txflow.schema.json` is the JSON Schema (draft 2020-12) for version `N` of the
experiment spec format, the `version:` field every spec starts with. The site serves
each one at `https://txflow.fun/schema/v<N>.json`, its `$id`, for editors and validators.

It is generated, never edited by hand. The single source is the pattern catalogue in
[`crates/txflow-core/src/catalogue.rs`](../crates/txflow-core/src/catalogue.rs): pattern
names, their arguments, ranges and defaults. The pattern code reads its ranges and
defaults from the same catalogue, so the schema and the expander cannot disagree. The
format version is `VERSION` in
[`crates/txflow-core/src/spec.rs`](../crates/txflow-core/src/spec.rs).

```sh
UPDATE_SCHEMA=1 cargo test -p txflow-core --test catalogue   # regenerate
cargo run -p txflow -- schema                                 # print it
```

A test fails when the committed schema for the current version differs from what the
code generates.

## When to bump the version

- **Same version, regenerate:** adding a pattern, adding an optional argument, widening
  a range, improving descriptions. Every spec that was valid stays valid and means the
  same thing.
  Publish it as a revision of the same version: tag `spec-v<N>.<r>` (for example
  `spec-v0.1`), which releases the regenerated `schema/v<N>/` files. The `version:`
  field stays `N`.
- **New version:** removing or renaming a pattern or argument, narrowing a range,
  changing a default, or changing what an existing spec expands to. Bump `VERSION`,
  regenerate (the new file lands in `schema/v<N+1>/`) and keep `schema/v<N>/` as it is,
  so older specs can still be validated and their meaning looked up.

The schema checks shape only. Whether a spec is runnable (references resolve, no output
spent twice along one path, size limits) is checked by `txflow check`, or in the browser
by the WebAssembly build of the same code.
