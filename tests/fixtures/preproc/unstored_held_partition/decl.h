#ifndef DECL_H
#define DECL_H
#include "types.h"
int leaf(env_t e);
#if defined(MODE) && MODE == 1
struct Session {
    int Inl(env_t e) { return leaf(e); }
};
#elif defined(MODE)
struct Other { int o; };
#else
struct Plain { int p; };
#endif
#endif
