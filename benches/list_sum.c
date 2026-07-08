#include <stdint.h>
#include <stdlib.h>

typedef struct Node {
    int64_t value;
    struct Node *next;
} Node;

int main(void) {
    Node *head = NULL;
    for (int64_t i = 0; i < 1000000; i++) {
        Node *n = malloc(sizeof(Node));
        n->value = i % 100;
        n->next = head;
        head = n;
    }
    int64_t total = 0;
    for (int pass = 0; pass < 10; pass++) {
        for (Node *cur = head; cur; cur = cur->next) {
            total += cur->value;
        }
    }
    return (int)(total % 251);
}
