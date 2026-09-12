void alpha(void) {}
void beta(void) {}
#if MODE == 1
#define TARGET alpha
#else
#define TARGET beta
#endif
static void header_entry(void) { TARGET(); }
