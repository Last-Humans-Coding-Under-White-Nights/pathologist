// ... and an unrelated one a namespace of the same name, whose free function
// has no `this` whatever the other program declares.
typedef void (*Callback)();

namespace ns {
namespace Clock {
void Format(Callback cb) { cb(); }
}
}

void OnFormat() {}

void UseClockNamespace() { ns::Clock::Format(OnFormat); }
