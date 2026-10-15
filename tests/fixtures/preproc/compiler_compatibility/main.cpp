#if defined(__clang__) && !defined(__GNUC__) && !defined(__GNUG__)
void ms_compatible(void) {}
#elif defined(__clang__) && defined(__GNUC__) && defined(__GNUG__)
void gnu_compatible(void) {}
#else
#error unexpected compiler identities
#endif
