.PHONY: all build test clean fmt fmt-check tidy

LLVM_BIN := /opt/homebrew/opt/llvm@22/bin
BUILD := ../build/cpp

NINJA := $(BUILD)/build.ninja
# The compile commands come from AppleClang, which finds the SDK by itself;
# LLVM's clang-tidy needs to be told.
TIDY_SYSROOT := $(if $(filter Darwin,$(shell uname)),-extra-arg=-isysroot$(shell xcrun --show-sdk-path))
TIDY_STAMP := $(BUILD)/.tidy.stamp
FMT_STAMP := $(BUILD)/.fmt.stamp
FMT_CHECK_STAMP := $(BUILD)/.fmt_check.stamp

# Ours only: not generated/ or third_party/.
FMT_SRCS := $(wildcard include/rpc/*.h src/*.cpp tests/*.cpp tests/*.h)
SRCS := $(FMT_SRCS) $(wildcard generated/rpc/*.h) CMakeLists.txt \
        tests/CMakeLists.txt .clang-tidy

all: test

$(NINJA): CMakeLists.txt tests/CMakeLists.txt
	cmake -S . -B $(BUILD) -G Ninja -DCMAKE_BUILD_TYPE=Debug

build: $(NINJA)
	cmake --build $(BUILD)

test: build
	ctest --test-dir $(BUILD) --output-on-failure --progress

tidy: $(TIDY_STAMP)

$(TIDY_STAMP): $(SRCS) | $(NINJA)
	@PATH="$(LLVM_BIN):$$PATH" $(LLVM_BIN)/run-clang-tidy \
		-clang-tidy-binary=$(LLVM_BIN)/clang-tidy \
		-clang-apply-replacements-binary=$(LLVM_BIN)/clang-apply-replacements \
		-p $(BUILD) \
		-quiet \
		-hide-progress \
		-source-filter '.*/cpp/(src|tests)/.*' \
		-header-filter '.*/cpp/(include|src|tests)/.*\.h$$' \
		$(TIDY_SYSROOT)
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
