#include <errno.h>
#include <stdlib.h>
#include <string.h>

#include "gcr_mcs.h"

#define LEVELDB_DIRECT_GCR_CACHE_LINE_SIZE 64

typedef struct mcs_tas_accordin_direct_mutex {
  gcr_mcs_mutex_t lock;
} mcs_tas_accordin_direct_mutex;

static int leveldb_direct_gcr_mutex_check(
    mcs_tas_accordin_direct_mutex *mutex) {
  return mutex == NULL ? EINVAL : 0;
}

mcs_tas_accordin_direct_mutex *mcs_tas_accordin_direct_mutex_create(void) {
  mcs_tas_accordin_direct_mutex *mutex = NULL;
  if (posix_memalign((void **)&mutex, LEVELDB_DIRECT_GCR_CACHE_LINE_SIZE,
                     sizeof(*mutex)) != 0) {
    return NULL;
  }

  memset(mutex, 0, sizeof(*mutex));
  gcr_mcs_init(&mutex->lock);
  return mutex;
}

int mcs_tas_accordin_direct_mutex_destroy(
    mcs_tas_accordin_direct_mutex *mutex) {
  int ret = leveldb_direct_gcr_mutex_check(mutex);
  if (ret != 0) {
    return ret;
  }

  gcr_mcs_destroy(&mutex->lock);
  free(mutex);
  return 0;
}

int mcs_tas_accordin_direct_mutex_lock(
    mcs_tas_accordin_direct_mutex *mutex) {
  int ret = leveldb_direct_gcr_mutex_check(mutex);
  if (ret != 0) {
    return ret;
  }

  gcr_mcs_lock(&mutex->lock);
  return 0;
}

int mcs_tas_accordin_direct_mutex_unlock(
    mcs_tas_accordin_direct_mutex *mutex) {
  int ret = leveldb_direct_gcr_mutex_check(mutex);
  if (ret != 0) {
    return ret;
  }

  gcr_mcs_unlock(&mutex->lock);
  return 0;
}
