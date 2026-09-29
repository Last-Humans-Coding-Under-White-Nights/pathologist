#ifndef TDECL_H
#define TDECL_H
#include "decl.h"
#if defined(MODE) && MODE == 1
struct TestSession {
    int Inl(env_t e) { return CALL_LEAF(e); }
};
#elif defined(MODE)
struct TestOther { int o; };
#else
struct TestPlain { int p; };
#endif
#endif
