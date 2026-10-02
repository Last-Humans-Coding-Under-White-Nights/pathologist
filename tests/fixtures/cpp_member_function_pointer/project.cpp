#include "dep/api.hpp"

struct Mine {
    int m(double) const;
};

using MyMember = int (Mine::*)(double) const;
typedef int (Mine::*MyNamed)(double) const;

int main() { return after_member_pointer(); }
