#include <stdint.h>
#include <stdbool.h>

static bool escapes(double cr, double ci) {
    double zr = 0.0, zi = 0.0;
    for (int i = 0; i < 100; i++) {
        double zr2 = zr * zr - zi * zi + cr;
        zi = 2.0 * zr * zi + ci;
        zr = zr2;
        if (zr * zr + zi * zi > 4.0) return true;
    }
    return false;
}

int main(void) {
    int64_t inside = 0;
    double ci = -1.25;
    for (int y = 0; y < 400; y++) {
        double cr = -2.0;
        for (int x = 0; x < 400; x++) {
            if (!escapes(cr, ci)) inside++;
            cr += 0.00625;
        }
        ci += 0.00625;
    }
    return (int)(inside % 251);
}
