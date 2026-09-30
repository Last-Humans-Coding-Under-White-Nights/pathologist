/* Opens the class the way unit tests do. */
#define private public
#include "../klass.h"
#undef private

int Test(Counter &counter)
{
    counter.value_ = 1;
    return Twice(counter);
}
