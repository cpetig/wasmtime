# Statically linkable Preview2 adapter

Fork of wasmtime to make the preview2 adapter statically linkable, to
enable creating preview2 modules instead of components.

To create the adapter 
```
cargo build --target wasm32-unknown-unknown -p wasi-preview1-component-adapter --features command --no-default-features
```

A new test case was added in `tests/preview2-adapter`, showing how
to create a preview2 module from wasi-SDK.
