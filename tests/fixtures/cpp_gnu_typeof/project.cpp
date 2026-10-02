#include "dep/api.hpp"

template<class T>
struct Mine {
    typedef __typeof__(T() + 1) type;
    typedef __typeof(T *) ptype;
    void mine();
};

int main() { return after_typeof(); }
