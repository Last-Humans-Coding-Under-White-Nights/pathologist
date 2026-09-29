/* Reaches decl.h through e2.h alone. */
#include "mode1.h"
#include "e2.h"
int use(Session *session, env_t env) { return session->Add(env); }
