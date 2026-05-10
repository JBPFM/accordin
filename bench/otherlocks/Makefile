CC ?= gcc
AR ?= ar

REPO_ROOT := $(abspath ../..)
LIBVSYNC_DIR := $(REPO_ROOT)/third_party/libvsync
LIBVSYNC_GIT_URL := https://github.com/open-s4c/libvsync.git
BUILD_DIR := build

CPPFLAGS ?=
CFLAGS ?= -O3 -std=gnu11 -fPIC -Wall -Wextra -pthread
LDFLAGS ?=
LDLIBS ?= -ldl -pthread

COMMON_SOURCES := otherlocks_pthread_interpose.c
GCR_SOURCES := $(COMMON_SOURCES) gcr_mcs.c
CNA_SOURCES := $(COMMON_SOURCES)

.PHONY: all clean test

all: $(BUILD_DIR)/interpose_gcr.sh $(BUILD_DIR)/interpose_cna.sh
test: $(BUILD_DIR)/gcr_mcs_empty_active_wakeup_test
	$<

$(BUILD_DIR):
	mkdir -p $@

$(BUILD_DIR)/interpose_gcr.so: $(GCR_SOURCES) gcr_mcs.h | $(BUILD_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) -DOTHERLOCK_KIND_GCR $(GCR_SOURCES) -shared -o $@ $(LDFLAGS) $(LDLIBS)

$(BUILD_DIR)/gcr_mcs_empty_active_wakeup_test: tests/gcr_mcs_empty_active_wakeup_test.c gcr_mcs.c gcr_mcs.h | $(BUILD_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) tests/gcr_mcs_empty_active_wakeup_test.c gcr_mcs.c -o $@ $(LDFLAGS) -pthread

$(LIBVSYNC_DIR)/include/vsync/spinlock/cnalock.h:
	mkdir -p $(dir $(LIBVSYNC_DIR))
	git clone $(LIBVSYNC_GIT_URL) $(LIBVSYNC_DIR)

$(BUILD_DIR)/interpose_cna.so: $(CNA_SOURCES) $(LIBVSYNC_DIR)/include/vsync/spinlock/cnalock.h | $(BUILD_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) -DOTHERLOCK_KIND_CNA -I$(LIBVSYNC_DIR)/include -I$(LIBVSYNC_DIR)/vatomic/include $(CNA_SOURCES) -shared -o $@ $(LDFLAGS) $(LDLIBS)

$(BUILD_DIR)/interpose_%.sh: interpose.in $(BUILD_DIR)/interpose_%.so | $(BUILD_DIR)
	sed -e "s|@base_dir@|$(abspath .)|g" -e "s|@lock_name@|$*|g" $< > $@
	chmod a+x $@

clean:
	rm -rf $(BUILD_DIR)
