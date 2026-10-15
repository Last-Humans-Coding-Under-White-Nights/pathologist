// The class header is unavailable, as in a partial repository checkout.
namespace app {
struct Callbacks { void run(); };
// A namespace function of the same name must not displace the owner's set.
double GetCallbacks();
int Context::GetCallbacks() { return 0; }
Callbacks Context::GetCallbacks(int) { return {}; }
void Context::Dispatch() {
    auto callbacks = GetCallbacks();
    auto other = GetCallbacks(1);
    other.run();
}
}
