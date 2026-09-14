// The definitions of `Detached`'s members, in a unit whose include of the
// class header is not found: they are lowered without their class in view.
#include "detached_not_in_tree.hpp"

typedef void (*Callback)();

void Detached::Later(Callback cb) { cb(); }
void Detached::Shared(Callback cb) { cb(); }
void Detached::Reset() {}

void OnSameUnit() {}

// A caller beside the definitions: its call binds to them while they still
// lack `this`.
void CallInImplUnit() { Detached::Shared(OnSameUnit); }
