
#define WEAK __attribute__((__weak__))
WEAK void leading(void) {}
void trailing(void) __attribute__((weak));
void trailing(void) {}
WEAK int leading_value = 2;
int value __attribute__((weak)) = 1;
#pragma weak pragma_fn
void pragma_fn(void) {}
int pragma_value;
#pragma weak pragma_value
#if 0
#pragma weak strong
#endif
void strong(void) { const char *s = "weak"; }

void alias_name(void);
#pragma weak alias_name = strong
void string_attribute(void) __attribute__((alias("weak")));

void late(void) {}
void late(void) __attribute__((weak));
