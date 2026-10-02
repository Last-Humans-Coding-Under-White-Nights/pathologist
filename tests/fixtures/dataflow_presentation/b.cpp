#include "common.hpp"

// Same names as a.cpp, but this storage and this function belong to b.cpp.
static char *saved;

static char *process(char *payload) {
    char *via_header = header_identity(payload);
    saved = via_header;
    return saved;
}

char *from_b(char *payload) {
    char *result = process(payload);
    return result;
}

void consume_b(char *payload) {
    global_saved = from_b(payload);
}

char *saved_in_b() {
    return saved;
}
