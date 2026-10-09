#ifndef TEST_DATAFLOW_MACROS_HPP
#define TEST_DATAFLOW_MACROS_HPP

// A function definition generated at the invocation site.
#define DEFINE_RECEIVER(name, destination) \
    static void name(char *payload) { destination = payload; }

// One ordinary function call per macro invocation.
#define INVOKE_RECEIVER(value) normal_receiver(value)

#endif
