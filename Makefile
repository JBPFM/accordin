# SPDX-License-Identifier: GPL-2.0-only
CC = clang
SMOKE_CC ?= cc
SMOKE_CXX ?= c++
BPF_CLANG ?= clang
BPFTOOL ?= bpftool
PKG_CONFIG ?= pkg-config
OUT ?= target/release
ARCH ?= $(shell uname -m | sed -e 's/aarch64/arm64/' -e 's/x86_64/x86/' -e 's/riscv64/riscv/' -e 's/ppc64le/powerpc/' -e 's/s390x/s390/')
VMLINUX_BTF ?= /sys/kernel/btf/vmlinux
CFLAGS ?= -O3 -g
CPPFLAGS += -D_GNU_SOURCE -Iinclude -Isrc -I$(OUT) -isystem third_party/scx
LIBBPF_CFLAGS := $(shell $(PKG_CONFIG) --cflags libbpf)
LIBBPF_LIBS := $(shell $(PKG_CONFIG) --libs libbpf)
HOST_FLAGS := -std=gnu11 -fPIC -fvisibility=hidden -Wall -Wextra -Werror $(LIBBPF_CFLAGS)
# The interposers are always preloaded, never dlopened, so their thread-local
# state can use the cheaper initial-exec model.
HOOK_FLAGS := -DACCORDIN_FULLHOOK -ftls-model=initial-exec
SMOKE_FLAGS := -O2 -Wall -Wextra -Werror -pthread
BPF_FLAGS := -target bpf -mcpu=v3 -g -O2 -D__TARGET_ARCH_$(ARCH) -I$(OUT) -isystem third_party/scx $(LIBBPF_CFLAGS)
MULTIARCH := $(shell gcc -print-multiarch 2>/dev/null)
ifneq ($(MULTIARCH),)
BPF_FLAGS += -idirafter /usr/include/$(MULTIARCH)
endif
ifeq ($(PERF_SYMBOLS),1)
CPPFLAGS += -DPERF_SYMBOLS
endif

BACKENDS := mcs_accordin_direct mcs_tas_accordin_direct
HOOKS := mcs_accordin_fullhook mcs_tas_accordin_fullhook
LIBRARIES := $(BACKENDS:%=$(OUT)/lib%.so) $(HOOKS:%=$(OUT)/lib%.so)
SMOKES := $(OUT)/direct_api_smoke $(OUT)/fullhook_smoke $(OUT)/fullhook_cxx_smoke
HEADERS := $(wildcard src/*.h include/*.h src/bpf/*.h third_party/scx/scx/*.h) Makefile

.PHONY: all $(BACKENDS) $(HOOKS) check check-bpf clean compile-commands
.DELETE_ON_ERROR:
all: $(LIBRARIES)
$(BACKENDS): %: $(OUT)/lib%.so
$(HOOKS): %: $(OUT)/lib%.so

$(OUT):
	mkdir -p $@

$(OUT)/vmlinux.h: $(VMLINUX_BTF) | $(OUT)
	$(BPFTOOL) btf dump file $< format c > $@.tmp
	mv $@.tmp $@

$(OUT)/accordin.bpf.o: src/bpf/main.bpf.c $(HEADERS) $(OUT)/vmlinux.h
	$(BPF_CLANG) $(BPF_FLAGS) -c $< -o $@

$(OUT)/accordin.skel.h: $(OUT)/accordin.bpf.o
	$(BPFTOOL) gen skeleton $< name accordin > $@.tmp
	mv $@.tmp $@

# One recipe for all four libraries: $(1) name, $(2) API source, $(3) defines.
define library_rule
$(OUT)/lib$(1).so: $(2) src/runtime.c $$(HEADERS) $(OUT)/accordin.skel.h
	$$(CC) $$(CPPFLAGS) $$(CFLAGS) $$(HOST_FLAGS) $(3) -shared $(2) src/runtime.c -o $$@ $$(LDFLAGS) -Wl,-z,defs $$(LIBBPF_LIBS) -pthread
endef

$(eval $(call library_rule,mcs_accordin_direct,src/direct.c,))
$(eval $(call library_rule,mcs_tas_accordin_direct,src/direct.c,-DMCS_TAS))
$(eval $(call library_rule,mcs_accordin_fullhook,src/fullhook.c,$(HOOK_FLAGS)))
$(eval $(call library_rule,mcs_tas_accordin_fullhook,src/fullhook.c,-DMCS_TAS $(HOOK_FLAGS)))

$(OUT)/direct_api_smoke: scripts/tests/direct_api_smoke.c | $(OUT)
	$(SMOKE_CC) -std=c11 $(SMOKE_FLAGS) $< -ldl -o $@

$(OUT)/fullhook_smoke: scripts/tests/fullhook_smoke.c | $(OUT)
	$(SMOKE_CC) -std=c11 $(SMOKE_FLAGS) $< -ldl -o $@

$(OUT)/fullhook_cxx_smoke: scripts/tests/fullhook_cxx_smoke.cc | $(OUT)
	$(SMOKE_CXX) -std=c++17 $(SMOKE_FLAGS) $< -ldl -o $@

SMOKE_VARS = SMOKE_BIN_DIR=$(abspath $(OUT)) DIRECT_LIB_DIR=$(abspath $(OUT)) \
	     FULLHOOK_LIB_DIR=$(abspath $(OUT))

check: all $(SMOKES)
	bash scripts/check_symbols.sh $(OUT)
	$(SMOKE_VARS) bash scripts/test_direct_api.sh --no-bpf
	$(SMOKE_VARS) bash scripts/test_fullhook.sh --no-bpf

check-bpf: all $(SMOKES)
	$(SMOKE_VARS) bash scripts/test_direct_api.sh --bpf
	$(SMOKE_VARS) bash scripts/test_fullhook.sh --bpf

compile-commands: all
	bash gen-compile-commands.sh

clean:
	rm -f $(LIBRARIES) $(SMOKES) $(OUT)/accordin.bpf.o $(OUT)/accordin.skel.h $(OUT)/vmlinux.h
