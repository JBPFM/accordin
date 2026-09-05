/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef ACCORDIN_ENV_FLAG_H
#define ACCORDIN_ENV_FLAG_H

#include <ctype.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>

/* Switch spelling shared by the runtime loader and the pthread interposer. */
static inline bool env_flag(const char *name)
{
    const char *value = getenv(name);
    if (!value)
        return false;
    while (isspace((unsigned char)*value))
        value++;
    size_t len = strlen(value);
    while (len && isspace((unsigned char)value[len - 1]))
        len--;
    return (len == 1 && *value == '1') ||
           (len == 4 && !strncasecmp(value, "true", len)) ||
           (len == 3 && !strncasecmp(value, "yes", len)) ||
           (len == 2 && !strncasecmp(value, "on", len));
}

#endif
