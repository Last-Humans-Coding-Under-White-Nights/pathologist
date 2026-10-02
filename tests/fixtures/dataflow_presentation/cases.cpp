#include "common.hpp"

static char default_text[] = "default";
char *global_saved;
char *default_payload = default_text;
char *default_output;

void consume_default(char *payload) {
    global_saved = payload;
}

struct Envelope {
    char *payload;
};

// Three parameters. Every omitted argument has its own default.
static void dispatch(char *payload = default_payload,
                     Handler handler = consume_default,
                     char **out = &default_output) {
    char *assigned = payload;
    char *copied = assigned;
    Envelope envelope;
    envelope.payload = copied;
    *out = envelope.payload;
    handler(copied);
}

// A different function with the same name in the SAME TU: an overload.
static char *dispatch(Envelope *envelope) {
    return envelope->payload;
}

namespace alternate {
// Same basename, distinguished by namespace as well as function identity.
static char *dispatch(char *payload) {
    return payload;
}
}

static char *recursive_identity(char *value, int remaining) {
    if (remaining == 0) return value;
    return recursive_identity(value, remaining - 1);
}

static void optional_pointer(char *payload = nullptr) {
    global_saved = payload;
}

extern "C" int run_cpp_cases(void) {
    static char first_text[] = "first";
    static char second_text[] = "second";
    char *first = first_text;
    char *second = second_text;
    Handler first_handler = consume_a;
    Handler second_handler = consume_b;
    char *first_out = nullptr;
    char *second_out = nullptr;

    optional_pointer();
    optional_pointer(first);
    optional_pointer(nullptr);

    dispatch(first, first_handler, &first_out);
    dispatch(second, second_handler, &second_out);
    dispatch(first, first_handler); // Default output parameter.
    dispatch(second);               // Default handler AND output parameters.
    dispatch();                     // All three default arguments.

    Envelope envelope;
    envelope.payload = first;
    char *overloaded_result = dispatch(&envelope);
    char *namespaced_result = alternate::dispatch(second);
    char *recursive_result = recursive_identity(first, 2);

    // Exercise both file-static process functions and both header instances.
    char *a_result = from_a(first);
    char *b_result = from_b(second);

    // Real execution validates C++ defaults and isolation of file-static state.
    if (first_out != first || second_out != second) return 1;
    if (default_output != default_payload || global_saved != default_payload) return 2;
    if (overloaded_result != first || namespaced_result != second) return 3;
    if (recursive_result != first) return 4;
    if (a_result != first || b_result != second) return 5;
    if (saved_in_a() != first || saved_in_b() != second) return 6;
    return 0;
}
