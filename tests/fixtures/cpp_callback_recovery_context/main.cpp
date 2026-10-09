struct T { void operator()() {} };
struct MemberShadow {
    ::T (*T)(int);
    void run(int *p) { T(*p)(); }
};
struct Callable { void operator()() {} };
Callable maker(int) { return {}; }
template<auto make> void value_template(int *p) { make(*p)(); }
void value_caller(int *p) { value_template<maker>(p); }
extern "C" int external_variable, external_post(void (*cb)());
extern "C" { int defined_variable, defined_post(void (*cb)()); }
struct S { S(int) {} ~S() {} void run() {} };
void job() {}
using F = void (*)();
F factory() { return job; }
void objects(int n) { S obj(n), object_post(void (*cb)()); }
void callbacks() { F cb(job), callback_post(void (*callback)()); cb(); }
void initialized() {
    F value = factory(), initialized_post(void (*cb)());
    value();
}
template<class U> U dependent_caller() {
    S local_post(void (*cb)());
    auto x = local_post(job);
    x.run();
    return U();
}
template<class T> T dependent_post(void (*cb)());
template<int N> struct Box {};
template<int N> void type_with_value_argument() { Box<N> (*box_callback)(); }
