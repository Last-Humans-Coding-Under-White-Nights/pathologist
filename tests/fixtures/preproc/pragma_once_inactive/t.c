/* Same header, reached with the conditional inactive: the `#pragma once`
   sits in a skipped group and states nothing, so the second `#include`
   re-expands. This is the other half of pragma_once_active, and it has to
   be its own run — there, the `#pragma once` holds for the whole TU. */
#define BODY first_body
#define TARGET first_target
#include "once.h"

#undef BODY
#undef TARGET
#define BODY second_body
#define TARGET second_target
#include "once.h"

void t_main(void) {
    first_body();
    second_body();
}
