#ifndef DECL_H
#define DECL_H
#include "types.h"
#ifdef MODE
struct Session {
    virtual int Add(env_t env);
};
struct Scan : Session {
    int Add(env_t env) override;
};
#else
struct Plain {
    int Add(env_t env);
};
#endif
#endif
