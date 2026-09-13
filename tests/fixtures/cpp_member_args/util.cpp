typedef void (*Callback)();

namespace util {
void Free(Callback cb) { cb(); }
void Init();
}

// A namespace function defined out of line: parameterless, qualified, and no
// member of anything.
void util::Init() {}
