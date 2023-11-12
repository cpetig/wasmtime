CC=/opt/wasi-sdk/bin/clang
CFLAGS=--target=wasm32-wasi -g -O0

test.wasm: main.c
	$(CC) $(CFLAGS) -o $@ $^ -lc -L. -lwasi_snapshot_preview1 
