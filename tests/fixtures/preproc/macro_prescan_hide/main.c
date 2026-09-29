void f(int value) { (void)value; }
#define f(x) x
#define ARGS (1)
void caller(void) { f(f ARGS); }

#define ID(x) x
#if ID(ID(1))
void expected_if(void) {}
#else
void wrong_if(void) {}
#endif

#if 0
void wrong_first_arm(void) {}
#elif ID(ID(1))
void expected_elif(void) {}
#else
void wrong_elif(void) {}
#endif
