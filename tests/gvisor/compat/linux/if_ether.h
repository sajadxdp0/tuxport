/* musl's <netinet/if_ether.h> already defines struct ethhdr and the ETH_P_* protocols, which
   the kernel's <linux/if_ether.h> would define a second time. */
#pragma once
#include <netinet/if_ether.h>
