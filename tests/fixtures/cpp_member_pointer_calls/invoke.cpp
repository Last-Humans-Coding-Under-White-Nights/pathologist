#include "api.hpp"
int *Worker::run(int *value, Hook callback) { callback(); field = value; return value; }
int *Worker::simple(int *value) { return value; }
int *Worker::wrong(double value) { return nullptr; }
void Worker::wrong_return(int *value) {}
int *Other::simple(int *value) { return value; }
int *invoke(Worker *p, Callback f, int *value, Hook callback) {
    int *result = (p->*f)(value, callback);
    return result;
}
int *Base::run(int *value) { return value; }
int *Base::run(double value) { return nullptr; }
int *Derived::run(int *value) { return value; }
int *Derived::run(double value) { return nullptr; }
int *dispatch(Base *p, Virtual f, int *value) { int *result = (p->*f)(value); return result; }
int *final_dispatch(Final *p, Virtual f, int *value) { return (p->*f)(value); }
int *FinalOverride::run(int *value) { return value; }
int *final_override(FinalOverride *p, Virtual f, int *value) { return (p->*f)(value); }
