#include "api.hpp"
#define MEMBER_CALL(object, member, ...) (object->*member)(__VA_ARGS__)
int global;
Worker object;
void hook() {}
void local(Worker *p, int *value) {
    Callback original = &Worker::run;
    Callback f = original;
    int *(Worker::*named)(int *, Hook) = &Worker::run;
    int *named_result = (p->*named)(value, hook);
    int *local_result = (p->*f)(value, hook);
    int *macro_result = MEMBER_CALL(p, f, value, hook);
}
void dot(int *value) {
    Worker obj;
    Callback f = &Worker::run;
    int *dot_result = (obj.*f)(value, hook);
}
void reference(Worker &obj, int *value) {
    Callback f = &Worker::run;
    int *reference_result = (obj.*f)(value, hook);
}
void negative(Worker *p, int *value) {
    Simple f = &Worker::simple;
    f = reinterpret_cast<Simple>(&Other::simple);
    f = reinterpret_cast<Simple>(&Worker::wrong);
    f = reinterpret_cast<Simple>(&Worker::wrong_return);
    int *negative_result = (p->*f)(value);
}
void source() {
    int *cross_result = invoke(&object, &Worker::run, &global, hook);
    local(&object, &global);
    dot(&global);
    reference(object, &global);
    negative(&object, &global);
    int *header_result = header_call(&object, &Worker::run, &global);
    Base base;
    int *virtual_result = dispatch(&base, &Base::run, &global);
    FinalOverride overriding;
    int *override_result = final_override(&overriding, &Base::run, &global);
    Final final_object;
    int *final_result = final_dispatch(&final_object, &Base::run, &global);
}
