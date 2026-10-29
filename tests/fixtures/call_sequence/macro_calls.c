#include "macros.h"

void step(void);
int a(void);
int b(void);
void log_it(int v);

void twice(void)
{
    TWICE(step());
}

void logs(void)
{
    LOG(a());
    LOG(b());
}
