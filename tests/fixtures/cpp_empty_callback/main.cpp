#include "api.hpp"
void job() {}
void (*callback)();
void (*initialized)() = job;
static void (*file_callback)() = job;
extern void (*declared_callback)();
void (*first_callback)() = job, (*second_callback)() = job;
static void local_post(void (*cb)());
void defined_post(void (*cb)()) { cb(); }
void caller() {
    a_post(job);
    b_post(job);
    c_post(nullptr);
    external_post(job);
    c_linked_post(job);
    api::post(job);
    api::defaults();
    api::first(job);
    api::second(job);
    api::qualified(job);
    result_post(result_job);
    local_post(job);
    void block_post(void (*cb)());
    block_post(job);
    void (*local_callback)() = job;
    static void (*static_callback)() = job;
    callback = job;
    callback();
    initialized();
    local_callback();
    static_callback();
    first_callback();
    second_callback();
    Callback alias_callback = job;
    CallbackAlias using_callback = job;
    alias_callback();
    using_callback();
    Result (*result_callback)() = result_job;
    result_callback();
}
