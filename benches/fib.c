#include <stdint.h>

static int64_t fib(int64_t n) {
    if (n < 2) return n;
    return fib(n - 1) + fib(n - 2);
}

int main(void) {
    return (int)(fib(34) % 251);
}
