#include "klass.h"

int Counter::Next() { return ++value_; }
int Production(Counter &counter) { return Twice(counter); }
