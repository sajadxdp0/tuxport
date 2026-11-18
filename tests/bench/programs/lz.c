/*
 * A simple LZ77: finds matches with a 4-byte hash and encodes greedily, then decodes to verify.
 * Memory-access and branch heavy; stands in for gzip (after M1 the real busybox gzip is measured
 * too). Usage: lz [rounds]
 *
 * Encoding: a token with the top bit 0 is followed by (token + 1) literal bytes; a top bit of 1
 * means a match of length (token & 127) + 4, followed by a 2-byte backward distance.
 */
#include "rt.h"

#define TEXT_BYTES (256 * 1024)
#define HASH_BITS 14
#define MAX_MATCH (127 + 4)

static uint8_t text[TEXT_BYTES];
static uint8_t packed[TEXT_BYTES + TEXT_BYTES / 64 + 16];
static uint8_t unpacked[TEXT_BYTES];
static uint32_t table[1 << HASH_BITS];

static const char *const words[] = {
    "the",   "tuxport", "alpine", "linux",  "android", "kernel", "page",  "table", "memory",
    "cpu",   "block",  "branch", "cache",  "signal",  "thread", "file",  "inode", "socket",
    "shell", "busybox", "musl",  "binary", "translate", "a",    "of",    "and",   "to",
};

static uint32_t next_random(uint32_t *s)
{
    *s = *s * 1103515245u + 12345u;
    return *s >> 8;
}

static void generate_text(uint32_t seed)
{
    size_t n = 0;
    while (n < TEXT_BYTES) {
        const char *w = words[next_random(&seed) % (sizeof(words) / sizeof(words[0]))];
        while (*w && n < TEXT_BYTES)
            text[n++] = (uint8_t)*w++;
        if (n < TEXT_BYTES)
            text[n++] = next_random(&seed) % 11 == 0 ? '\n' : ' ';
    }
}

static uint32_t load32(const uint8_t *p)
{
    uint32_t v;
    memcpy(&v, p, 4);
    return v;
}

static size_t flush_literals(const uint8_t *in, size_t from, size_t to, uint8_t *out, size_t o)
{
    while (from < to) {
        size_t run = to - from > 128 ? 128 : to - from;
        out[o++] = (uint8_t)(run - 1);
        memcpy(out + o, in + from, run);
        o += run;
        from += run;
    }
    return o;
}

static size_t compress(const uint8_t *in, size_t n, uint8_t *out)
{
    memset(table, 0, sizeof(table));
    size_t i = 0, literal_start = 0, o = 0;
    while (i + 4 <= n) {
        uint32_t v = load32(in + i);
        uint32_t h = (v * 2654435761u) >> (32 - HASH_BITS);
        uint32_t candidate = table[h];
        table[h] = (uint32_t)i + 1;
        if (candidate && i - (candidate - 1) < 65536 && load32(in + candidate - 1) == v) {
            size_t m = candidate - 1, len = 4;
            while (i + len < n && len < MAX_MATCH && in[m + len] == in[i + len])
                len++;
            o = flush_literals(in, literal_start, i, out, o);
            size_t distance = i - m;
            out[o++] = (uint8_t)(0x80 | (len - 4));
            out[o++] = (uint8_t)distance;
            out[o++] = (uint8_t)(distance >> 8);
            i += len;
            literal_start = i;
        } else {
            i++;
        }
    }
    return flush_literals(in, literal_start, n, out, o);
}

static size_t decompress(const uint8_t *in, size_t n, uint8_t *out)
{
    size_t i = 0, o = 0;
    while (i < n) {
        uint8_t token = in[i++];
        if (token & 0x80) {
            size_t len = (size_t)(token & 127) + 4;
            size_t distance = in[i] | (size_t)in[i + 1] << 8;
            i += 2;
            for (size_t k = 0; k < len; k++, o++)
                out[o] = out[o - distance];
        } else {
            size_t run = (size_t)token + 1;
            memcpy(out + o, in + i, run);
            i += run;
            o += run;
        }
    }
    return o;
}

int main(int argc, char **argv)
{
    uint64_t rounds = rt_arg_u64(argc, argv, 1, 4);
    uint64_t total = 0, checksum = 0xcbf29ce484222325u;
    for (uint64_t r = 0; r < rounds; r++) {
        generate_text((uint32_t)r + 1);
        size_t packed_len = compress(text, TEXT_BYTES, packed);
        if (decompress(packed, packed_len, unpacked) != TEXT_BYTES ||
            memcmp(text, unpacked, TEXT_BYTES) != 0)
            rt_fail("lz round trip");
        total += packed_len;
        for (size_t i = 0; i < packed_len; i += 61)
            checksum = (checksum ^ packed[i]) * 0x100000001b3u;
    }
    rt_print("lz ");
    rt_print_u64(total);
    rt_print(" ");
    rt_print_u64(checksum);
    rt_print("\n");
    return 0;
}
