CC=/opt/wasi-sdk/bin/clang
CFLAGS=--target=wasm32-wasi -g -O0

all: component2.wasm

test.wasm: main.c
	$(CC) $(CFLAGS) -o $@ $^ -lc -L. -lwasi_snapshot_preview1 '-Wl,--export=wasi:cli/run@0.2.0#run' -Wl,--export=cabi_realloc

preview1.wasm: main.c
	$(CC) $(CFLAGS) -o $@ $^

component.wasm: preview1.wasm
	wasm-tools component new $^ --adapt wasi_snapshot_preview1.wasm -o $@

component2.wasm: test.wasm
	wasm-tools component new $^ -o $@

clean:
	rm test.wasm preview1.wasm component.wasm component2.wasm
