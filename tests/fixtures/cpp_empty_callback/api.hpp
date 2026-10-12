#pragma once
#define CALLBACK(name) void (*name)()
void a_post(CALLBACK(cb));
void b_post(void (*cb)(void));
void c_post(int (*cb)(int));
extern void external_post(void (*cb)());
extern "C" void c_linked_post(void (*cb)());
namespace api {
void post(void (*cb)());
void defaults(void (*cb)() = nullptr);
void first(void (*cb)()), second(void (*cb)());
void qualified(void (*cb)());
}
void api::qualified(void (*cb)());
struct Result {};
Result result_job();
void result_post(Result (*cb)());
namespace types {
struct Result {};
void post(Result (*cb)());
}
typedef void (*Callback)();
using CallbackAlias = void (*)();
struct Queue { void post(void (*cb)()); };
void job();
