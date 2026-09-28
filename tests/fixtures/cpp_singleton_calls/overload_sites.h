#pragma once
// R10-1: a header body's call bound to two overloads keeps both sites.
struct R10Pick {
    static void Pick(int v);
    static void Pick(const char *s);
    static void Use() { Pick(r10_unknown()); }
};
