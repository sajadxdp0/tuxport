/* Included before every source of the gVisor syscall tests (scripts/build-gvisor-tests): the
   glibc extensions they use that musl lacks. */
#pragma once
#include <errno.h>
/* glibc's headers bring in <sys/types.h> on the way; some test headers use mode_t without it */
#include <sys/types.h>
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

#ifdef __cplusplus
#include <signal.h>
#include <sys/wait.h>

#ifndef W_EXITCODE
#define W_EXITCODE(ret, sig) ((ret) << 8 | (sig))
#endif
#ifndef MSG_COPY
#define MSG_COPY 040000
#endif
/* glibc's names for the signal event types, and its enum for ptrace requests (musl takes an int) */
typedef struct sigevent sigevent_t;
typedef union sigval sigval_t;
typedef int __ptrace_request;
#endif
