// How `std::jthread` and `std::async` line their arguments up with the
// callable they start. As in the corpora, the standard headers are not in
// the tree: `std::jthread`, `std::stop_token` and `std::launch` are names
// the unit never sees declared.
#include <future>
#include <stop_token>
#include <thread>

typedef void (*Callback)();

// `std::jthread` hands a callable that takes a `std::stop_token` first its
// own token, ahead of the arguments it was given.
void Hit() {}
void Work(std::stop_token token, Callback cb) { cb(); }
void StartStoppable() { std::jthread t(Work, Hit); }

// Without one, it forwards the arguments as `std::thread` does.
void PlainHit() {}
void Plain(Callback cb) { cb(); }
void StartPlain() { std::jthread t(Plain, PlainHit); }

// `std::async` without a launch policy runs its first argument; the
// function after it is only an argument, which `AsyncWork` never calls.
void Unused() {}
void AsyncWork(Callback cb) {}
void StartAsync() { auto f = std::async(AsyncWork, Unused); }

// With a launch policy, the callable comes second.
void PolicyHit() {}
void PolicyWork(Callback cb) { cb(); }
void StartPolicy() { auto f = std::async(std::launch::async, PolicyWork, PolicyHit); }

// A policy held in a `std::launch` variable is a policy too.
void HeldHit() {}
void HeldWork(Callback cb) { cb(); }
void StartHeld(std::launch policy) { auto f = std::async(policy, HeldWork, HeldHit); }

// A token or a policy taken by reference is one too: a reference is lowered
// as its referent's address, which the class tests see through.
void RefHit() {}
void RefWork(const std::stop_token &token, Callback cb) { cb(); }
void StartRefStoppable() { std::jthread t(RefWork, RefHit); }
void RvalHit() {}
void RvalWork(std::stop_token &&token, Callback cb) { cb(); }
void StartRvalStoppable() { std::jthread t(RvalWork, RvalHit); }
void RefHeldHit() {}
void RefHeldWork(Callback cb) { cb(); }
void StartRefHeld(const std::launch &policy) { auto f = std::async(policy, RefHeldWork, RefHeldHit); }
void LocalRefHit() {}
void LocalRefWork(Callback cb) { cb(); }
void StartLocalRef(std::launch held) {
    const std::launch &policy = held;
    auto f = std::async(policy, LocalRefWork, LocalRefHit);
}

// A pointer to a token is no token: `std::jthread` forwards it.
void PtrHit() {}
void PtrWork(std::stop_token *token, Callback cb) { cb(); }
void StartPtr(std::stop_token *tp) { std::jthread t(PtrWork, tp, PtrHit); }
