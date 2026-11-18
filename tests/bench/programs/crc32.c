/* Table-driven CRC-32: one table lookup per byte, memory-read heavy. Usage: crc32 [rounds] */
#include "rt.h"

static uint32_t table[256];
static uint8_t buf[64 * 1024];

static uint32_t crc32(uint32_t crc, const uint8_t *p, size_t n)
{
    crc = ~crc;
    for (size_t i = 0; i < n; i++)
        crc = table[(crc ^ p[i]) & 0xff] ^ (crc >> 8);
    return ~crc;
}

int main(int argc, char **argv)
{
    for (uint32_t i = 0; i < 256; i++) {
        uint32_t c = i;
        for (int k = 0; k < 8; k++)
            c = c & 1 ? 0xedb88320u ^ (c >> 1) : c >> 1;
        table[i] = c;
    }
    if (crc32(0, (const uint8_t *)"123456789", 9) != 0xcbf43926u)
        rt_fail("crc32 self-test");

    uint64_t rounds = rt_arg_u64(argc, argv, 1, 100);
    for (size_t i = 0; i < sizeof(buf); i++)
        buf[i] = (uint8_t)(i * 7 + (i >> 8));
    uint32_t crc = 0;
    for (uint64_t r = 0; r < rounds; r++) {
        crc = crc32(crc, buf, sizeof(buf));
        buf[r % sizeof(buf)] ^= (uint8_t)crc;
    }
    rt_print("crc32 ");
    rt_print_u64(crc);
    rt_print("\n");
    return 0;
}
