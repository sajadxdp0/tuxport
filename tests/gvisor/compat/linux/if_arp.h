/* musl's <net/if_arp.h> (which its <netinet/if_ether.h> includes) already defines struct arpreq
   and the ARPHRD_* types, which the kernel's <linux/if_arp.h> would define a second time. */
#pragma once
#include <net/if_arp.h>
