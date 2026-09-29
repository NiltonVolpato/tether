.PHONY: all clean

FLATC := flatc
RUSTFMT := rustup run nightly rustfmt --edition 2024 --config-path .rustfmt.toml
BUILD := build
GEN := tests/generated

all: src/rpc_generated.rs $(GEN)/coprocessor_generated.rs $(GEN)/coprocessor_rpc.rs

src/rpc_generated.rs: schema/rpc.fbs schema/status.fbs
	$(FLATC) --rust --gen-all -o src/ schema/rpc.fbs
	$(RUSTFMT) $@

$(GEN)/coprocessor_generated.rs: schema/coprocessor.fbs schema/rpc_attributes.fbs
	$(FLATC) --rust --gen-object-api --gen-all -o $(GEN)/ schema/coprocessor.fbs
	$(RUSTFMT) $@

$(BUILD)/%.json: schema/%.fbs schema/rpc_attributes.fbs rpcgen/reflection.fbs
	$(FLATC) -b --schema --bfbs-builtins --bfbs-comments -o $(BUILD)/ $<
	$(FLATC) --json --strict-json --raw-binary -o $(BUILD)/ rpcgen/reflection.fbs -- $(BUILD)/$*.bfbs

$(GEN)/coprocessor_rpc.rs: $(BUILD)/coprocessor.json $(wildcard rpcgen/src/*.rs)
	cargo run --offline -q -p rpcgen -- $< --types crate::generated::coprocessor_generated > $@.tmp
	mv $@.tmp $@
	$(RUSTFMT) $@

clean:
	rm -rf $(BUILD) $(GEN)/*_generated.rs $(GEN)/*_rpc.rs
