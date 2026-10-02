#include "common.hpp"

// These names deliberately also exist in b.cpp: distinct internal symbols.
static char *saved;

static char *process(char *payload) {
    char *via_header = header_identity(payload);
    saved = via_header;
    return saved;
}

char *from_a(char *payload) {
    char *result = process(payload);
    return result;
}

void consume_a(char *payload) {
    global_saved = from_a(payload);
}

char *saved_in_a() {
    return saved;
}
