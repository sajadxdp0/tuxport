/* Included before every source of the Starnix syscall tests (scripts/build-starnix-tests): the
   glibc extensions they use that musl lacks. */
#pragma once
#include <errno.h>
#include <unistd.h>

#ifndef TEMP_FAILURE_RETRY
#define TEMP_FAILURE_RETRY(expression)                                                             \
    (__extension__({                                                                               \
        long int __result;                                                                         \
        do                                                                                         \
            __result = (long int)(expression);                                                     \
        while (__result == -1L && errno == EINTR);                                                 \
        __result;                                                                                  \
    }))
#endif
