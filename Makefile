.PHONY: all clean

FLATC := flatc
RUSTFMT := rustup run nightly rustfmt --edition 2024 --config-path .rustfmt.toml
BUILD := build
GEN := core/tests/generated
CPP_GEN := cpp/tether/generated/tether
CPP_TEST_GEN := cpp/tests/generated
CPP_APP_GEN := cpp/test_app/main/generated
CLANG_FORMAT := /opt/homebrew/opt/llvm@22/bin/clang-format

all: core/src/wire_generated.rs $(CPP_GEN)/wire_generated.h $(GEN)/coprocessor_generated.rs $(GEN)/coprocessor_rpc.rs \
     $(CPP_TEST_GEN)/coprocessor_generated.h $(CPP_TEST_GEN)/coprocessor_rpc.h \
     $(GEN)/greeter_generated.rs $(GEN)/greeter_rpc.rs \
     $(CPP_APP_GEN)/greeter_generated.h $(CPP_APP_GEN)/greeter_rpc.h

core/src/wire_generated.rs: schema/wire.fbs schema/status.fbs
	$(FLATC) --rust --gen-all -o core/src/ schema/wire.fbs
	$(RUSTFMT) $@

$(CPP_GEN)/wire_generated.h: schema/wire.fbs schema/status.fbs
	$(FLATC) --cpp --cpp-std c++17 --scoped-enums --gen-all -o $(CPP_GEN)/ schema/wire.fbs

# The test application's schema; it includes the framework's attributes.
$(GEN)/coprocessor_generated.rs: core/tests/coprocessor.fbs schema/tether.fbs
	$(FLATC) --rust --gen-object-api --gen-all -I schema -o $(GEN)/ $<
	$(RUSTFMT) $@

$(GEN)/coprocessor_rpc.rs: core/tests/coprocessor.fbs schema/tether.fbs $(wildcard gen/src/*)
	cargo run --offline -q -p tether-gen -- $< -I schema --types crate::generated::coprocessor_generated > $@.tmp
	mv $@.tmp $@
	$(RUSTFMT) $@

# The same schema for the C++ server's host tests. The generated header includes
# no other generated one (--gen-all), so `tether.fbs`' attributes need none.
$(CPP_TEST_GEN)/coprocessor_generated.h: core/tests/coprocessor.fbs schema/tether.fbs
	$(FLATC) --cpp --cpp-std c++17 --scoped-enums --gen-all -I schema -o $(CPP_TEST_GEN)/ $<

$(CPP_TEST_GEN)/coprocessor_rpc.h: core/tests/coprocessor.fbs schema/tether.fbs $(wildcard gen/src/*)
	cargo run --offline -q -p tether-gen -- $< -I schema --lang cpp --include coprocessor_generated.h > $@.tmp
	mv $@.tmp $@
	$(CLANG_FORMAT) -i $@

# The integration test's service: served by the C++ test app, called from Rust.
$(GEN)/greeter_generated.rs: cpp/test_app/greeter.fbs schema/tether.fbs
	$(FLATC) --rust --gen-object-api --gen-all -I schema -o $(GEN)/ $<
	$(RUSTFMT) $@

$(GEN)/greeter_rpc.rs: cpp/test_app/greeter.fbs schema/tether.fbs $(wildcard gen/src/*)
	cargo run --offline -q -p tether-gen -- $< -I schema --types crate::generated::greeter_generated > $@.tmp
	mv $@.tmp $@
	$(RUSTFMT) $@

$(CPP_APP_GEN)/greeter_generated.h: cpp/test_app/greeter.fbs schema/tether.fbs
	$(FLATC) --cpp --cpp-std c++17 --scoped-enums --gen-all -I schema -o $(CPP_APP_GEN)/ $<

$(CPP_APP_GEN)/greeter_rpc.h: cpp/test_app/greeter.fbs schema/tether.fbs $(wildcard gen/src/*)
	cargo run --offline -q -p tether-gen -- $< -I schema --lang cpp --include greeter_generated.h > $@.tmp
	mv $@.tmp $@
	$(CLANG_FORMAT) -i $@

clean:
	rm -rf $(BUILD) $(GEN)/*_generated.rs $(GEN)/*_rpc.rs
