#include "base.h"

struct Derived : Base {
    void run() override;
};

void Derived::run() {}
