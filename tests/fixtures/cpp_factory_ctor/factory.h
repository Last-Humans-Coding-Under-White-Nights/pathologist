// Shared by main.cpp, other.cpp and third.cpp (#192). The inline function
// is a header-origin entity every unit lowers: its factory constructor
// sites and their heap temporaries must deduplicate at merge.
#pragma once
#include <memory>

void Handler();
void Other();

struct Foo {
    Foo(int a, void (*h)());
};

inline void from_header() {
    auto a = std::make_shared<Foo>(1, Handler);
    auto b = std::make_unique<Foo>(2, Handler);
}
