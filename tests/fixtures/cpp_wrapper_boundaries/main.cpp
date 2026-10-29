#include <external.hpp>
class Item { public: int Run() { return 1; } };
class Other { public: int Run() { return 2; } };
class Result { public: int Run() { return 3; } };
Result global;
template<class T> struct Fancy {
    Fancy(T *) {}
    Result *operator->() { return &global; }
};
template<template<class> class sptr>
int dispatch(Item *item) {
    auto result = sptr<Item>(item);
    return result->Run();
}
int use(Item *item) { return dispatch<Fancy>(item); }
int nested(Other *other) {
    auto result = lib::Outer<Item>::sptr<Other>(other);
    return result->Run();
}
int nested_chain(Other *other) {
    return lib::Outer<Item>::sptr<Other>(other)->Run();
}
int typed_control(Other *other) {
    lib::Outer<Item>::sptr<Other> result(other);
    return result->Run();
}
