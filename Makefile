.PHONY: all clean

FLATC := flatc
RUSTFMT := rustup run nightly rustfmt --edition 2024 --config-path .rustfmt.toml
BUILD := build
GEN := core/tests/generated

all: core/src/rpc_generated.rs $(GEN)/coprocessor_generated.rs $(GEN)/coprocessor_rpc.rs

core/src/rpc_generated.rs: schema/rpc.fbs schema/status.fbs
	$(FLATC) --rust --gen-all -o core/src/ schema/rpc.fbs
	$(RUSTFMT) $@

# The test application's schema; it includes the framework's attributes.
$(GEN)/coprocessor_generated.rs: core/tests/coprocessor.fbs schema/rpc_attributes.fbs
	$(FLATC) --rust --gen-object-api --gen-all -I schema -o $(GEN)/ $<
	$(RUSTFMT) $@

$(BUILD)/%.bfbs: core/tests/%.fbs schema/rpc_attributes.fbs
	$(FLATC) -b --schema --bfbs-builtins --bfbs-comments -I schema -o $(BUILD)/ $<

$(GEN)/coprocessor_rpc.rs: $(BUILD)/coprocessor.bfbs $(wildcard rpcgen/src/*.rs)
	cargo run --offline -q -p rpcgen -- $< --types crate::generated::coprocessor_generated > $@.tmp
	mv $@.tmp $@
	$(RUSTFMT) $@

clean:
	rm -rf $(BUILD) $(GEN)/*_generated.rs $(GEN)/*_rpc.rs
