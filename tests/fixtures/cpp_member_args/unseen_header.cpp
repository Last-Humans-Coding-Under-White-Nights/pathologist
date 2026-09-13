// A caller whose include of the class header is not found: the qualified
// call has no candidate while this unit is lowered, and resolves by name to
// the member definition only after the merge.
#include "remote_not_in_tree.hpp"

typedef void (*Callback)();

void OnUnseen() {}
void OnUnseenFree() {}
void OnDeclaredOnly() {}

void CallWithoutHeader() {
    Remote::Shared(OnUnseen);
    util::Free(OnUnseenFree);
    Remote::Declared(OnDeclaredOnly);
}
