#include "macros.hpp"

static char *outer_saved;
static char *inner_saved;
static char *generated_saved;
static char *called_saved;

DEFINE_RECEIVER(generated_receiver, generated_saved)

static void normal_receiver(char *payload) {
    called_saved = payload;
}

static void shadowing(char *first, char *second) {
    char *p = first; // Outer p: its own VarId.
    outer_saved = p;
    {
        char *p = second; // Inner p: a different VarId.
        inner_saved = p;
    }
    outer_saved = p; // The outer binding is visible again.
}

extern "C" int run_macro_cases(void) {
    static char first_text[] = "macro-first";
    static char second_text[] = "macro-second";
    char *first = first_text;
    char *second = second_text;

    shadowing(first, second);
    if (outer_saved != first || inner_saved != second) return 1;

    generated_receiver(first);
    if (generated_saved != first) return 2;

    INVOKE_RECEIVER(first);
    if (called_saved != first) return 3;
    INVOKE_RECEIVER(second);
    if (called_saved != second) return 4;
    return 0;
}
