/* e1.h holds decl.h under MODE 1; e2.h then skips it by its guard. */
#include "mode1.h"
#include "e1.h"
#include "e2.h"
int b(Session *s, env_t e) { return s->Add(e); }
