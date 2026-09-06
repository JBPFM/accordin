# SPDX-License-Identifier: GPL-2.0-only
CC = clang
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
BPF_FLAGS := -target bpf -mcpu=v3 -g -O2 -D__TARGET_ARCH_$(ARCH) -I$(OUT) -isystem third_party/scx $(LIBBPF_CFLAGS)
MULTIARCH := $(shell gcc -print-multiarch 2>/dev/null)
ifneq ($(MULTIARCH),)
BPF_FLAGS += -idirafter /usr/include/$(MULTIARCH)
endif
ifeq ($(PERF_SYMBOLS),1)
CPPFLAGS += -DPERF_SYMBOLS
endif

BACKENDS := mcs_accordin_direct mcs_tas_accordin_direct
LIBRARIES := $(BACKENDS:%=$(OUT)/lib%.so)
HEADERS := $(wildcard src/*.h include/*.h src/bpf/*.h third_party/scx/scx/*.h) Makefile

.PHONY: all $(BACKENDS) check check-bpf litl check-litl check-litl-bpf clean compile-commands
.DELETE_ON_ERROR:
all: $(LIBRARIES)
$(BACKENDS): %: $(OUT)/lib%.so

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

$(OUT)/libmcs_accordin_direct.so: src/direct.c src/runtime.c $(HEADERS) $(OUT)/accordin.skel.h
	$(CC) $(CPPFLAGS) $(CFLAGS) $(HOST_FLAGS) -shared src/direct.c src/runtime.c -o $@ $(LDFLAGS) -Wl,-z,defs $(LIBBPF_LIBS) -pthread

$(OUT)/libmcs_tas_accordin_direct.so: src/direct.c src/runtime.c $(HEADERS) $(OUT)/accordin.skel.h
	$(CC) $(CPPFLAGS) $(CFLAGS) $(HOST_FLAGS) -DMCS_TAS -shared src/direct.c src/runtime.c -o $@ $(LDFLAGS) -Wl,-z,defs $(LIBBPF_LIBS) -pthread

check: all
	bash scripts/check_direct_symbols.sh $(OUT)
	DIRECT_LIB_DIR=$(abspath $(OUT)) bash scripts/test_direct_api.sh --no-bpf

check-bpf: all
	DIRECT_LIB_DIR=$(abspath $(OUT)) bash scripts/test_direct_api.sh --bpf

litl: all
	$(MAKE) -C third_party/litl ACCORDIN_ROOT=$(CURDIR) ACCORDIN_LIB_DIR=$(abspath $(OUT)) ALGORITHMS="mcsaccordin_original mcstasaccordin_original" all

check-litl: litl
	cd third_party/litl && ACCORDIN_ROOT=$(CURDIR) ACCORDIN_LIB_DIR=$(abspath $(OUT)) bash tests/run.sh --no-bpf

check-litl-bpf: litl
	cd third_party/litl && ACCORDIN_ROOT=$(CURDIR) ACCORDIN_LIB_DIR=$(abspath $(OUT)) bash tests/run.sh --bpf

compile-commands: all
	bash gen-compile-commands.sh

clean:
	rm -f $(LIBRARIES) $(OUT)/accordin.bpf.o $(OUT)/accordin.skel.h $(OUT)/vmlinux.h
