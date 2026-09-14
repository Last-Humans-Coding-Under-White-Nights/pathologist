#include "listener.hpp"

void Listener::Fire(const char *name, Callback cb) const { (void)name; cb(); }
