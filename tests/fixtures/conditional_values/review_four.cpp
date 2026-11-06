using FourCallback = void (*)();
void alpha();
void beta();
FourCallback four_reference = alpha;
struct FourCallable {
    FourCallback &operator()() { return four_reference; }
};
decltype(auto) four_callable_wrap(bool c) {
    FourCallable a, b;
    return (c ? a : b)();
}
decltype(auto) four_lambda_wrap() {
    auto f = []() -> FourCallback & { return four_reference; };
    return f();
}
void four_callable_reference(bool c) {
    FourCallback *p = &four_callable_wrap(c);
    (*p)();
}
void four_lambda_reference() {
    FourCallback *p = &four_lambda_wrap();
    (*p)();
}
struct FourA { FourCallback a; FourCallback c; };
struct FourB { FourCallback b; FourCallback d; };
union FourUnion { FourA x; FourB y; };
FourUnion four_table[1] = {{{alpha, beta}}};
void four_union_first() { four_table[0].y.b(); }
void four_union_second() { four_table[0].y.d(); }
void four_union_write() {
    struct WriteA { FourCallback a; FourCallback c; };
    struct WriteB { FourCallback b; FourCallback d; };
    union WriteUnion { WriteA x; WriteB y; };
    WriteUnion local[1] = {{{alpha, beta}}};
    local[0].y.b = beta;
    local[0].x.a();
}
struct FourPrefixA { FourCallback a; int stop; FourCallback tail; };
struct FourPrefixB { FourCallback b; double stop; FourCallback tail; };
union FourPrefixUnion { FourPrefixA x; FourPrefixB y; };
FourPrefixUnion four_prefix[1] = {{{alpha, 0, beta}}};
void four_union_prefix() { four_prefix[0].y.b(); }
// Beyond the compatible prefix, no alias to x.tail is established.
void four_union_incompatible_tail() { four_prefix[0].y.tail(); }
struct FourMembers { FourCallback first; FourCallback second; };
FourMembers four_members[1] = {{alpha, beta}};
void four_first_member_cast() {
    FourCallback *p = reinterpret_cast<FourCallback *>(four_members);
    FourCallback f = *p;
    f();
}
struct FourSingleMember { FourCallback first; int second; };
FourSingleMember four_single[1] = {{alpha, 0}};
void four_single_member_cast() {
    FourCallback *p = reinterpret_cast<FourCallback *>(four_single);
    (*p)();
}
