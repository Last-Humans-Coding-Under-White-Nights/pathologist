#include "remote.hpp"

void Remote::Later(Callback cb) { cb(); }
void Remote::Shared(Callback cb) { cb(); }
