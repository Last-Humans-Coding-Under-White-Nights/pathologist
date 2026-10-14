#if defined(__clang__) && defined(__GNUC__) && defined(__STRICT_ANSI__) && TEST_STANDARD == TEST_EXPECTED_STANDARD
void selected_standard(void) {}
#else
#error missing compiler identity or incorrect language standard
#endif
