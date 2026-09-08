/* Diamond: t.c -> {left.h, right.h} -> base.h. base.h is guarded, so it is
   expanded once and the re-splice cost stays linear in the graph. */
#include "left.h"
#include "right.h"

void t_main(void) {
    left_fn();
    right_fn();
}
