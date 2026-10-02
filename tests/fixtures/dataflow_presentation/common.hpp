#ifndef TEST_DATAFLOW_COMMON_HPP
#define TEST_DATAFLOW_COMMON_HPP

using Handler = void (*)(char *);

extern char *global_saved;
extern char *default_payload;
extern char *default_output;

void consume_a(char *payload);
void consume_b(char *payload);
void consume_default(char *payload);

// Same spelling and source location, included in two different TUs.
static inline char *header_identity(char *value) {
    return value;
}

char *from_a(char *payload);
char *from_b(char *payload);
char *saved_in_a();
char *saved_in_b();

#endif
