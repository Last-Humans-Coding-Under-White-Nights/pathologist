#ifndef DECL_H
#define DECL_H
#include "types.h"
int leaf(env_t e);
/* Spelled here, called wherever it is invoked. */
#define CALL_LEAF(e) leaf(e)
#if defined(MODE) && MODE == 1
struct Session { int s; };
#elif defined(MODE)
struct Other { int o; };
#else
struct Plain { int p; };
#endif
#endif
