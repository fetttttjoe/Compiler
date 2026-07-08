#include <stdint.h>

int main(void) {
    int64_t sum = 0;
    for (int64_t i = 0; i < 200000000; i++) {
        sum = sum + i * 3 - i % 7;
    }
    return (int)(sum % 251);
}
