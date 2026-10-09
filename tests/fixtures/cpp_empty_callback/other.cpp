#include "api.hpp"
types::Result typed_job();
void other_caller(Queue &queue) {
    types::post(typed_job);
    queue.post(job);
}
