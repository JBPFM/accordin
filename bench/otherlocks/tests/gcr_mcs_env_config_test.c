#define _GNU_SOURCE
#include "../gcr_mcs.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

/*
 * The environment is parsed once per process, so each case runs in its own
 * child process image with the variables set before the first initialization.
 */

#define SELF_PATH "/proc/self/exe"

static int check_u32(const char *what, uint32_t got, uint32_t want) {
  if (got == want) {
    return 0;
  }
  fprintf(stderr, "%s: expected %u, got %u\n", what, want, got);
  return 1;
}

static int check_mutex(const char *what, uint32_t active_limit,
                       uint32_t rejoin_limit, uint32_t signal_period,
                       uint32_t passive_spins) {
  gcr_mcs_mutex_t lock;
  gcr_mcs_config_t config;
  int bad = 0;

  gcr_mcs_init(&lock);
  gcr_mcs_effective_config(&config);

  bad |= check_u32("config active_limit", config.active_limit, active_limit);
  bad |= check_u32("config rejoin_limit", config.rejoin_limit, rejoin_limit);
  bad |= check_u32("config signal_period", config.signal_period, signal_period);
  bad |= check_u32("config passive_spins", config.passive_spins, passive_spins);

  bad |= check_u32("mutex active_limit", lock.active_limit, active_limit);
  bad |= check_u32("mutex rejoin_limit", lock.rejoin_limit, rejoin_limit);
  bad |= check_u32("mutex signal_period", lock.signal_period, signal_period);
  bad |= check_u32("mutex passive_spins", lock.passive_spins, passive_spins);

  gcr_mcs_destroy(&lock);
  if (bad) {
    fprintf(stderr, "case %s failed\n", what);
  }
  return bad;
}

static int case_defaults(void) {
  return check_mutex("defaults", GCR_MCS_DEFAULT_ACTIVE_LIMIT, 2u,
                     GCR_MCS_DEFAULT_SIGNAL_PERIOD,
                     GCR_MCS_DEFAULT_PASSIVE_SPINS);
}

/* Decimal, hexadecimal, and a rejoin threshold derived from the override. */
static int case_valid(void) {
  return check_mutex("valid", 9u, 4u, 0x100u, 77u);
}

static int case_malformed(void) {
  return check_mutex("malformed", GCR_MCS_DEFAULT_ACTIVE_LIMIT, 2u,
                     GCR_MCS_DEFAULT_SIGNAL_PERIOD,
                     GCR_MCS_DEFAULT_PASSIVE_SPINS);
}

static int case_zero(void) {
  return check_mutex("zero", GCR_MCS_DEFAULT_ACTIVE_LIMIT, 2u,
                     GCR_MCS_DEFAULT_SIGNAL_PERIOD,
                     GCR_MCS_DEFAULT_PASSIVE_SPINS);
}

/* gcr_mcs_init_with ignores the environment: zero still means the default. */
static int case_explicit(void) {
  gcr_mcs_mutex_t lock;
  int bad = 0;

  gcr_mcs_init_with(&lock, 0u, 0u, 0u);
  bad |= check_u32("explicit default active_limit", lock.active_limit,
                   GCR_MCS_DEFAULT_ACTIVE_LIMIT);
  bad |= check_u32("explicit default rejoin_limit", lock.rejoin_limit, 2u);
  bad |= check_u32("explicit default signal_period", lock.signal_period,
                   GCR_MCS_DEFAULT_SIGNAL_PERIOD);
  bad |= check_u32("explicit default passive_spins", lock.passive_spins,
                   GCR_MCS_DEFAULT_PASSIVE_SPINS);

  gcr_mcs_init_with(&lock, 6u, 7u, 8u);
  bad |= check_u32("explicit active_limit", lock.active_limit, 6u);
  bad |= check_u32("explicit rejoin_limit", lock.rejoin_limit, 3u);
  bad |= check_u32("explicit signal_period", lock.signal_period, 7u);
  bad |= check_u32("explicit passive_spins", lock.passive_spins, 8u);

  gcr_mcs_destroy(&lock);
  if (bad) {
    fprintf(stderr, "case explicit failed\n");
  }
  return bad;
}

typedef struct env_pair {
  const char *name;
  const char *value;
} env_pair_t;

typedef struct test_case {
  const char *name;
  int (*run)(void);
  env_pair_t env[3];
} test_case_t;

static const test_case_t cases[] = {
    {"defaults", case_defaults, {{NULL, NULL}, {NULL, NULL}, {NULL, NULL}}},
    {"valid",
     case_valid,
     {{GCR_MCS_ENV_ACTIVE_LIMIT, "9"},
      {GCR_MCS_ENV_SIGNAL_PERIOD, "0x100"},
      {GCR_MCS_ENV_PASSIVE_SPINS, "77"}}},
    {"malformed",
     case_malformed,
     {{GCR_MCS_ENV_ACTIVE_LIMIT, "-1"},
      {GCR_MCS_ENV_SIGNAL_PERIOD, "12x"},
      {GCR_MCS_ENV_PASSIVE_SPINS, "0x"}}},
    {"zero",
     case_zero,
     {{GCR_MCS_ENV_ACTIVE_LIMIT, "0"},
      {GCR_MCS_ENV_SIGNAL_PERIOD, "0x0"},
      {GCR_MCS_ENV_PASSIVE_SPINS, "0"}}},
    {"explicit",
     case_explicit,
     {{GCR_MCS_ENV_ACTIVE_LIMIT, "3"},
      {GCR_MCS_ENV_SIGNAL_PERIOD, "5"},
      {GCR_MCS_ENV_PASSIVE_SPINS, "11"}}},
};

#define CASE_COUNT (sizeof(cases) / sizeof(cases[0]))

static int run_case_in_child(const test_case_t *tc) {
  pid_t pid = fork();
  if (pid < 0) {
    fprintf(stderr, "fork failed for case %s\n", tc->name);
    return 1;
  }

  if (pid == 0) {
    char *argv[] = {(char *)SELF_PATH, (char *)tc->name, NULL};
    for (size_t i = 0; i < sizeof(tc->env) / sizeof(tc->env[0]); ++i) {
      if (tc->env[i].name == NULL) {
        continue;
      }
      if (setenv(tc->env[i].name, tc->env[i].value, 1) != 0) {
        _exit(2);
      }
    }
    execv(SELF_PATH, argv);
    _exit(3);
  }

  int status = 0;
  if (waitpid(pid, &status, 0) < 0 || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    fprintf(stderr, "case %s did not exit cleanly (status %d)\n", tc->name,
            status);
    return 1;
  }
  return 0;
}

int main(int argc, char **argv) {
  if (argc > 1) {
    for (size_t i = 0; i < CASE_COUNT; ++i) {
      if (strcmp(argv[1], cases[i].name) == 0) {
        return cases[i].run() ? 1 : 0;
      }
    }
    fprintf(stderr, "unknown case %s\n", argv[1]);
    return 1;
  }

  /* The parent never initializes a lock, so every child starts unresolved. */
  int failures = 0;
  for (size_t i = 0; i < CASE_COUNT; ++i) {
    failures += run_case_in_child(&cases[i]);
  }
  return failures ? 1 : 0;
}
