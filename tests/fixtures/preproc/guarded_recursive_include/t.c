/* a.h and b.h include each other. Both are guarded, so the recursion
   terminates on the guard rather than on the include-depth cap — and
   neither body may be lost to it. */
#include "a.h"

void t_main(void) {
    a_fn();
    b_fn();
}
