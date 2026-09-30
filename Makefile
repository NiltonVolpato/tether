.PHONY: all clean

FLATC := flatc
RUSTFMT := rustup run nightly rustfmt --edition 2024 --config-path .rustfmt.toml
BUILD := build
GEN := core/tests/generated
CPP_GEN := cpp/tether/generated/tether

all: core/src/wire_generated.rs $(CPP_GEN)/wire_generated.h $(GEN)/coprocessor_generated.rs $(GEN)/coprocessor_rpc.rs

core/src/wire_generated.rs: schema/wire.fbs schema/status.fbs
	$(FLATC) --rust --gen-all -o core/src/ schema/wire.fbs
	$(RUSTFMT) $@

$(CPP_GEN)/wire_generated.h: schema/wire.fbs schema/status.fbs
	$(FLATC) --cpp --cpp-std c++17 --scoped-enums --gen-all -o $(CPP_GEN)/ schema/wire.fbs

# The test application's schema; it includes the framework's attributes.
$(GEN)/coprocessor_generated.rs: core/tests/coprocessor.fbs schema/tether.fbs
	$(FLATC) --rust --gen-object-api --gen-all -I schema -o $(GEN)/ $<
	$(RUSTFMT) $@

$(BUILD)/%.bfbs: core/tests/%.fbs schema/tether.fbs
	$(FLATC) -b --schema --bfbs-builtins --bfbs-comments -I schema -o $(BUILD)/ $<

$(GEN)/coprocessor_rpc.rs: $(BUILD)/coprocessor.bfbs $(wildcard gen/src/*.rs)
	cargo run --offline -q -p tether-gen -- $< --types crate::generated::coprocessor_generated > $@.tmp
	mv $@.tmp $@
	$(RUSTFMT) $@

clean:
	rm -rf $(BUILD) $(GEN)/*_generated.rs $(GEN)/*_rpc.rs
