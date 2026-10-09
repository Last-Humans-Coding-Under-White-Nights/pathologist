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

struct LocalTypeShadow {
    void (*T)(int);
    void run() { using T = void; T (*local_cb)() = job; local_cb(); }
};
struct BaseValue { void (*T)(int); };
struct DerivedTypeShadow : BaseValue {
    using T = void;
    void run() { T (*derived_cb)() = job; derived_cb(); }
};
struct TemplateTypeShadow {
    void (*T)(int);
    template<class T> void run() { T (*template_cb)(); }
};
struct Factory { static Callable make(int) { return {}; } };
template<class T> void dependent_member_call(int *p) { T::make(*p)(); }
void dependent_member_caller(int *p) { dependent_member_call<Factory>(p); }

struct ImportedFunction {};
struct ImportedVariable {};
namespace Imports {
    Callable ImportedFunction(int) { return {}; }
    Callable (*ImportedVariable)(int) = ImportedFunction;
    using Result = void;
}
struct Result {};
void body_imported_function(int *p) { using Imports::ImportedFunction; ImportedFunction(*p)(); }
void body_imported_variable(int *p) { using Imports::ImportedVariable; ImportedVariable(*p)(); }
namespace ImportScope {
    using Imports::ImportedFunction;
    void namespace_imported_function(int *p) { ImportedFunction(*p)(); }
}
void body_imported_type() { using Imports::Result; Result (*imported_cb)() = job; imported_cb(); }
