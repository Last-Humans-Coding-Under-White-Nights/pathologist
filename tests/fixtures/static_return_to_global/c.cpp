// The C++ grammar takes the same lowering path (issue #132: "C and C++ alike").
struct XC { int v; };
XC global_c;
static XC *src_c = &global_c;

static void *ret_static_cpp() { return src_c; }

void *cpp_seen;
static void *cpp_seen_static;
void *cpp_seen_init = ret_static_cpp();

void use_c() {
    cpp_seen = ret_static_cpp();
    cpp_seen_static = ret_static_cpp();
}
