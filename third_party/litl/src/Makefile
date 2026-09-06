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
