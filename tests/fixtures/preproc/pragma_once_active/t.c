/* The `#pragma once` is reached with its conditional active, so it takes
   effect for the rest of this translation unit — a later change to
   ENABLE_ONCE cannot undo it, and the second `#include` expands nothing. */
#define ENABLE_ONCE 1
#define BODY first_body
#define TARGET first_target
#include "once.h"

#undef ENABLE_ONCE
#undef BODY
#undef TARGET
#define BODY second_body
#define TARGET second_target
#include "once.h"

void t_main(void) { first_body(); }
