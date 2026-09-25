#include "anon.h"

// Dispatch from another file reaches the anonymous class's override.
void DispatchAnon(IAnon *a) { a->Handle(); }
