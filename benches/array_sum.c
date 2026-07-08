#include <stdint.h>
#include <stdlib.h>

/* Same growable-array shape the ys runtime uses: doubling realloc. */
int main(void) {
    int64_t len = 0, cap = 0;
    int64_t *data = NULL;
    for (int64_t i = 0; i < 5000000; i++) {
        if (len == cap) {
            cap = cap ? cap * 2 : 4;
            data = realloc(data, (size_t)cap * 8);
        }
        data[len++] = i % 1000;
    }
    int64_t total = 0;
    for (int pass = 0; pass < 10; pass++) {
        for (int64_t j = 0; j < len; j++) {
            total += data[j];
        }
    }
    return (int)(total % 251);
}
