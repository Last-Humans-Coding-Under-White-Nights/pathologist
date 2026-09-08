#include "lang.h"
#ifdef __cplusplus
extern "C" {
#endif
int f(void);
#if __cplusplus >= 201103L
int cxx11_only(void);
#endif
#ifdef __cplusplus
}
#endif
void b_main(void) { LANG_CALL(); }
