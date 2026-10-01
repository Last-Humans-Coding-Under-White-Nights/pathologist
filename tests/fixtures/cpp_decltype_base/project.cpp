#include "dep/api.hpp"

template<class T>
struct Mine : decltype(make<T>()) {
    void mine();
};

using Example = Derived<Base>;

int main() { return after_decltype(); }
