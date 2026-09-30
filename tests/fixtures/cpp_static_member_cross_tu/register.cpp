#include "holder.h"
void handler() {}
void setup(Holder *h) { h->cb = handler; }
