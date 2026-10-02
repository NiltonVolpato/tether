.PHONY: all build test qemu-test clean fmt fmt-check tidy

LLVM_BIN := /opt/homebrew/opt/llvm@22/bin
BUILD := ../build/cpp

NINJA := $(BUILD)/build.ninja
TIDY_STAMP := $(BUILD)/.tidy.stamp
FMT_STAMP := $(BUILD)/.fmt.stamp
FMT_CHECK_STAMP := $(BUILD)/.fmt_check.stamp

# Ours only: not tether/generated/ or tether/third_party/.
FMT_SRCS := $(wildcard tether/include/tether/*.h tether/src/*.cpp tests/*.cpp tests/*.h \
                      test_app/main/*.cpp test_app/main/*.h)
SRCS := $(FMT_SRCS) $(wildcard tether/generated/tether/*.h) CMakeLists.txt \
        tether/CMakeLists.txt tests/CMakeLists.txt .clang-tidy

all: test

$(NINJA): CMakeLists.txt tether/CMakeLists.txt tests/CMakeLists.txt
	cmake -S . -B $(BUILD) -G Ninja -DCMAKE_BUILD_TYPE=Debug

build: $(NINJA)
	cmake --build $(BUILD)

test: build
	ctest --test-dir $(BUILD) --output-on-failure --progress

# The integration test: the component on an emulated ESP32, the Rust core on
# the host. Needs ESP-IDF's environment (idf.py, QEMU).
qemu-test:
	test_app/qemu-test.sh

tidy: $(TIDY_STAMP)

$(TIDY_STAMP): $(SRCS) | $(NINJA)
	@PATH="$(LLVM_BIN):$$PATH" $(LLVM_BIN)/run-clang-tidy \
		-clang-tidy-binary=$(LLVM_BIN)/clang-tidy \
		-clang-apply-replacements-binary=$(LLVM_BIN)/clang-apply-replacements \
		-p $(BUILD) \
		-quiet \
		-hide-progress \
		-source-filter '.*/cpp/(tether/src|tests)/.*' \
		-header-filter '.*/cpp/(tether/(include|src)/.*|tests/([^/]*|generated/.*_rpc))\.h$$'
	@touch $(TIDY_STAMP)

fmt: $(FMT_STAMP)

$(FMT_STAMP): $(FMT_SRCS) | $(NINJA)
	$(LLVM_BIN)/clang-format -i $(FMT_SRCS)
	@touch $(FMT_STAMP) $(FMT_CHECK_STAMP)

fmt-check: $(FMT_CHECK_STAMP)

$(FMT_CHECK_STAMP): $(FMT_SRCS) | $(NINJA)
	$(LLVM_BIN)/clang-format --dry-run --Werror $(FMT_SRCS)
	@touch $(FMT_CHECK_STAMP)

clean:
	rm -rf $(BUILD)
