/* `#undef`ing a header's own include guard and including it again must
   re-expand the body, under whatever macros are in force the second time. */
#define MAKE first_body
#define TARGET first_target
#include "body.h"
#undef MAKE
#undef TARGET

#undef BODY_H
#define MAKE second_body
#define TARGET second_target
#include "body.h"
#undef MAKE
#undef TARGET

void t_main(void) {
    first_body();
    second_body();
}
