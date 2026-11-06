typedef void (*Callback)(void);
static void local_alpha(void) {}
static void local_beta(void) {}
static Callback get_alpha(void) { return local_alpha; }
static Callback get_beta(void) { return local_beta; }
Callback cross_choose(int c) { return c ? get_alpha() : get_beta(); }
