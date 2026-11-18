/* musl's <sys/sysinfo.h> already defines struct sysinfo, which the kernel's <linux/sysinfo.h>
   (included by <linux/kernel.h>) would define a second time. */
#pragma once
#include <sys/sysinfo.h>
