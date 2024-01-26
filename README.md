# Statically linkable Preview2 adapter

Fork of wasmtime to make the preview2 adapter statically linkable, to
enable creating preview2 modules instead of components.

This is uses linker tricks present in WASI-SDK and isn't compatible with
Rust.

To create the adapter 
```
cargo build --target wasm32-unknown-unknown -p wasi-preview1-component-adapter --features command --no-default-features
```

A new test case was added in `tests/preview2-adapter`, showing how
to create a preview2 module from wasi-SDK.

You can run the module converted into a component via exactly the same
wasmtime version as the adapter:
```
cd tests/preview2-adapter
make component2.wasm
cargo run -- -S preview2 component2.wasm
```

PS: Make sure to add 
`-lc -L. -lwasi_snapshot_preview1 '-Wl,--export=wasi:cli/run@0.2.0#run' -Wl,--export=cabi_realloc` 
to your linker command line.
