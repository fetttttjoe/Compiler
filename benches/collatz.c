#include <stdint.h>

static int64_t steps(int64_t start) {
    int64_t n = start, s = 0;
    while (n != 1) {
        if (n % 2 == 0) n = n / 2; else n = 3 * n + 1;
        s++;
    }
    return s;
}

int main(void) {
    int64_t total = 0;
    for (int64_t i = 1; i < 300000; i++) {
        total += steps(i);
    }
    return (int)(total % 251);
}
