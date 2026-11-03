#pragma once
using Hook = void (*)();
struct Worker {
    int *field;
    int *run(int *value, Hook callback);
    int *simple(int *value);
    int *wrong(double value);
    void wrong_return(int *value);
};
struct Other { int *simple(int *value); };
using Callback = int *(Worker::*)(int *, Hook);
typedef int *(Worker::*Simple)(int *);
int *invoke(Worker *p, Callback f, int *value, Hook callback);
struct Base {
    virtual int *run(int *value);
    virtual int *run(double value);
};
struct Derived : Base { int *run(int *value) override; int *run(double value) override; };
struct Final final : Base {};
using Virtual = int *(Base::*)(int *);
int *dispatch(Base *p, Virtual f, int *value);
int *final_dispatch(Final *p, Virtual f, int *value);
void hook();
#define HEADER_MEMBER(p, f, value) (p->*f)(value, hook)
inline int *header_call(Worker *p, Callback f, int *value) {
    return HEADER_MEMBER(p, f, value);
}
struct FinalOverride final : Base { int *run(int *value) override; };
int *final_override(FinalOverride *p, Virtual f, int *value);
