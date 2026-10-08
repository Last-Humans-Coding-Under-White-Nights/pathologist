#include "types.hpp"

R::R(Holder &h, const Plain &p) : ref_(h), copy_(p) {}

void Handler() {}
void UseCallbackReference() {
    Callback cb = Handler;
    CallbackReference ref(cb);
    ref.Run();
}
