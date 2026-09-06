include ../Makefile.config

ACCORDIN_ROOT ?= $(abspath ../../..)
ACCORDIN_LIB_DIR ?= $(ACCORDIN_ROOT)/target/release
ACCORDIN_MAP=interpose.map
ifeq ($(shell uname -m),aarch64)
ACCORDIN_MAP=interpose-aarch64.map
endif
ACCORDIN_CFLAGS=-std=gnu11 -fPIC -Wall -Werror -O3 -g -I../include -I$(ACCORDIN_ROOT)/include $(EXTERNAL_CFLAGS)

.PHONY: accordin-force
accordin-force:

# Rebuild when switching compiler flags or direct library location.
../obj/%/accordin-config: accordin-force
	$(file >$@.tmp,$(CC) $(ACCORDIN_CFLAGS) LIB_DIR=$(ACCORDIN_LIB_DIR))
	@cmp -s $@.tmp $@ && rm $@.tmp || mv $@.tmp $@

define ACCORDIN_RULES
../obj/$(1)_original/interpose.o: interpose.c interpose.h utils.h waiting_policy.h ../include/accordin.h ../include/topology.h ../obj/$(1)_original/accordin-config Makefile
	$$(CC) $$(ACCORDIN_CFLAGS) -D$(2) -DFCT_LINK_SUFFIX=$(1) -DWAITING_ORIGINAL -c $$< -o $$@

../obj/$(1)_original/$(1)_original.o: accordin.c interpose.h ../include/accordin.h ../include/accordin-internal.h $$(ACCORDIN_ROOT)/include/$(3).h $$(ACCORDIN_ROOT)/include/accordin_relock.h ../obj/$(1)_original/accordin-config Makefile
	$$(CC) $$(ACCORDIN_CFLAGS) -D$(2) -DFCT_LINK_SUFFIX=$(1) -DWAITING_ORIGINAL -c $$< -o $$@

../obj/$(1)_original/accordin-cond.o: accordin-cond.c ../include/accordin.h ../include/accordin-internal.h $$(ACCORDIN_ROOT)/include/accordin_relock.h ../obj/$(1)_original/accordin-config Makefile
	$$(CC) $$(ACCORDIN_CFLAGS) -D$(2) -DFCT_LINK_SUFFIX=$(1) -c $$< -o $$@

../lib/lib$(1)_original.so: ../obj/$(1)_original/interpose.o ../obj/$(1)_original/$(1)_original.o ../obj/$(1)_original/accordin-cond.o $$(ACCORDIN_LIB_DIR)/lib$(3).so $$(ACCORDIN_MAP)
	$$(CC) -shared -o $$@ $$(filter %.o,$$^) -Wl,-z,defs,-z,now -Wl,--version-script=$$(ACCORDIN_MAP) -L$$(ACCORDIN_LIB_DIR) -Wl,-rpath,$$(ACCORDIN_LIB_DIR) -l$(3) -ldl -pthread
endef

$(eval $(call ACCORDIN_RULES,mcsaccordin,MCSACCORDIN,mcs_accordin_direct))
$(eval $(call ACCORDIN_RULES,mcstasaccordin,MCSTASACCORDIN,mcs_tas_accordin_direct))

# Baseline algorithms sharing the direct front end. Each one contributes its
# own sources and, when it wraps an external runtime, its own flags.
DIRECT_CFLAGS=-std=gnu11 -fPIC -Wall -Werror -O3 -g -I../include $(EXTERNAL_CFLAGS)
DIRECT_CXXFLAGS=-std=c++20 -fPIC -Wall -Werror -O3 -g -I../include $(EXTERNAL_CFLAGS)
DIRECT_HEADERS=interpose.h utils.h waiting_policy.h ../include/directalgo.h \
	../include/directlock.h ../include/topology.h
DIRECT_SHARED_SOURCES=interpose directlock directcond

gcr_DEFINE=GCR
gcr_CSOURCES=gcr gcrmcs
gcr_LINKER=$(CC)

LIBVSYNC_HEADER=$(LIBVSYNC_DIR)/include/vsync/spinlock/cnalock.h
cna_DEFINE=CNA
cna_CSOURCES=cna
cna_LINKER=$(CC)
cna_CFLAGS=-I$(LIBVSYNC_DIR)/include -I$(LIBVSYNC_DIR)/vatomic/include
cna_PREREQS=$(LIBVSYNC_HEADER)

$(LIBVSYNC_HEADER):
	[ -d $(LIBVSYNC_DIR) ] || git clone https://github.com/open-s4c/libvsync.git $(LIBVSYNC_DIR)

# FlexGuard keeps its lock, its queue nodes and its BPF runtime in an archive
# produced by the FlexGuard checkout, so the adapter has to agree with the
# archive on the lock layout: padding, spin queue flavour and BPF support are
# part of the structure definitions. FLEXGUARD_NOBPF=1 selects the userspace
# only variant, whose lock loads no BPF program at all.
FLEXGUARD_NOBPF ?= 0
FLEXGUARD_ARCHIVE=$(FLEXGUARD_DIR)/libsync.a
FLEXGUARD_BPF_ENABLED=$(filter 0,$(FLEXGUARD_NOBPF))
flexguard_DEFINE=FLEXGUARD
flexguard_CSOURCES=flexguard
flexguard_LINKER=$(CC)
flexguard_CFLAGS=-I$(FLEXGUARD_DIR)/include -DADD_PADDING -DHYBRID_MCS \
	$(if $(FLEXGUARD_BPF_ENABLED),-DBPF)
flexguard_LDFLAGS=$(FLEXGUARD_ARCHIVE) $(if $(FLEXGUARD_BPF_ENABLED),-lelf -lz)
flexguard_PREREQS=$(FLEXGUARD_ARCHIVE)

# The bundled libbpf predates the diagnostics the host compiler reports, so it
# is compiled without -Werror. CFLAGS travels through the environment: bpftool
# appends to it and would lose its own flags to a command line assignment.
$(FLEXGUARD_ARCHIVE):
	CFLAGS="-g -O2 -Wall -Wno-error" $(MAKE) -C $(FLEXGUARD_DIR) \
		NOBPF=$(FLEXGUARD_NOBPF) libsync.a

# The lock algorithms taken from the mutex microbenchmark keep their C++
# implementation and are compiled into the shared object.
MB_ALGORITHMS=mbmcs mbmcstas mbmcstastse mbmcstasnext mbmcstasnexttse \
	mbclh mbtwa mbhapax mbreciprocating

define MB_ALGORITHM_VARS
$(1)_DEFINE=$(shell echo $(1) | tr '[:lower:]' '[:upper:]')
$(1)_CXXSOURCES=$(1)
$(1)_LINKER=$$(CXX)
endef

$(foreach a,$(MB_ALGORITHMS),$(eval $(call MB_ALGORITHM_VARS,$(a))))

define DIRECT_C_RULE
../obj/$(1)_original/$(2).o: $(2).c $(DIRECT_HEADERS) $($(1)_PREREQS) Makefile ../Makefile.config
	$(CC) $(DIRECT_CFLAGS) $($(1)_CFLAGS) -DLITL_DIRECT_ALGORITHM -D$($(1)_DEFINE) \
		-DFCT_LINK_SUFFIX=$(1) -DWAITING_ORIGINAL -c $$< -o $$@
endef

define DIRECT_CXX_RULE
../obj/$(1)_original/$(2).o: $(2).cpp $(DIRECT_HEADERS) $($(1)_PREREQS) Makefile ../Makefile.config
	$(CXX) $(DIRECT_CXXFLAGS) $($(1)_CFLAGS) -DLITL_DIRECT_ALGORITHM -D$($(1)_DEFINE) \
		-DFCT_LINK_SUFFIX=$(1) -DWAITING_ORIGINAL -c $$< -o $$@
endef

define DIRECT_LINK_RULE
../lib/lib$(1)_original.so: $(addprefix ../obj/$(1)_original/,$(addsuffix .o,\
		$(DIRECT_SHARED_SOURCES) $($(1)_CSOURCES) $($(1)_CXXSOURCES))) \
		$($(1)_PREREQS) $(ACCORDIN_MAP)
	$($(1)_LINKER) -shared -o $$@ $$(filter %.o,$$^) -Wl,-z,defs,-z,now \
		-Wl,--version-script=$(ACCORDIN_MAP) $($(1)_LDFLAGS) -ldl -pthread
endef

$(foreach a,$(DIRECT_ALGORITHM_NAMES),\
	$(foreach c,$(DIRECT_SHARED_SOURCES) $($(a)_CSOURCES),\
		$(eval $(call DIRECT_C_RULE,$(a),$(c))))\
	$(foreach c,$($(a)_CXXSOURCES),\
		$(eval $(call DIRECT_CXX_RULE,$(a),$(c))))\
	$(eval $(call DIRECT_LINK_RULE,$(a))))

LDFLAGS=-L../obj/CLHT/external/lib -L../obj/CLHT -Wl,--whole-archive -Wl,--version-script=interpose.map -lsspfd -lssmem -lclht -Wl,--no-whole-archive  -lrt -lm -ldl -lpapi -m64 -pthread
CFLAGS=-I../include/ -I../obj/CLHT/include/ -I../obj/CLHT/external/include/ -fPIC -Wall -Werror -O3 -g

# Keep objects files
.PRECIOUS: %.o
.SECONDARY: $(OBJS)

.SECONDEXPANSION:
../obj/%.o: $$(lastword $$(subst /, ,%)).c $$(lastword $$(subst /, ,%)).h
	$(eval $@_TMP := $(shell echo $@ | cut -d/ -f3 | cut -d_ -f1))
	$(CC) $(CFLAGS) -D$$(echo $@ | cut -d/ -f3 | cut -d_ -f1 | tr '[a-z]' '[A-Z]') -DCOND_VAR=1 -DFCT_LINK_SUFFIX=$($@_TMP) -DWAITING_$$(echo $@ | cut -d/ -f3 | cut -d_ -f2- | tr '[a-z]' '[A-Z]') -o $@ -c $<

.SECONDEXPANSION:
../obj/%.o: $$(firstword $$(subst _, , $$(lastword $$(subst /, ,%)))).c ../include/$$(firstword $$(subst _, , $$(lastword $$(subst /, ,%)))).h
	$(eval $@_TMP := $(shell echo $@ | cut -d/ -f3 | cut -d_ -f1))
	$(CC) $(CFLAGS) -D$$(echo $@ | cut -d/ -f3 | cut -d_ -f1 | tr '[a-z]' '[A-Z]') -DCOND_VAR=1 -DFCT_LINK_SUFFIX=$($@_TMP) -DWAITING_$$(echo $@ | cut -d/ -f3 | cut -d_ -f2- | tr '[a-z]' '[A-Z]') -o $@ -c $<

.SECONDEXPANSION:
../lib/lib%.so: ../obj/%/interpose.o ../obj/%/utils.o $$(subst algo,%,../obj/algo/algo.o)
	$(CC) -shared -o $@ $^ $(LDFLAGS)
