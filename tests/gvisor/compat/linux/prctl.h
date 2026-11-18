/* musl's <sys/prctl.h> already defines the prctl constants and struct prctl_mm_map, which the
   kernel's <linux/prctl.h> would define a second time: a source including both does not compile.
   Found first on the include path when building the gVisor syscall tests. */
#pragma once
#include <sys/prctl.h>
