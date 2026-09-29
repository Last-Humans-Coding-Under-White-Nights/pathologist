#ifndef DECL_H
#define DECL_H
#include "types.h"
#if defined(MODE) && MODE == 1
struct Session {
    virtual int Add(env_t env);
};
struct Scan : Session {
    int Add(env_t env) override;
};
#elif defined(MODE)
struct Other {
    int Add(env_t env);
};
#else
struct Plain {
    int Add(env_t env);
};
#endif
#endif
