// `T w(a);` defines an object when `a` is not a type, though tree-sitter
// parses it as a function declaration.
typedef void (*Callback)();

void OnName() {}
void OnVar() {}
void OnFirst() {}
void OnSecond() {}
void OnBrace() {}
void OnPointer() {}
void OnAggregate() {}

class Worker {
public:
    Worker(Callback cb) { cb(); }
};

class Pair {
public:
    Pair(Callback first, Callback second) { first(); second(); }
};

struct Aggregate {
    Callback cb;
};

void ByName() { Worker w(OnName); }

void ByVariable() {
    Callback f = OnVar;
    Worker w(f);
}

void ByTwo() { Pair p(OnFirst, OnSecond); }

void ByBraces() { Worker w{OnBrace}; }

void PointerDirectInit() {
    Callback c(OnPointer);
    c();
}

void AggregateBraces() {
    Aggregate a{OnAggregate};
    a.cb();
}

// A defaulted constructor is not user-provided: the class is still an
// aggregate (C++17), and its braces initialize the field.
struct Defaulted {
    Defaulted() = default;
    Callback cb;
};

void OnDefaulted() {}

void DefaultedAggregate() {
    Defaulted d{OnDefaulted};
    d.cb();
}

// A local alias hides the global variable of the same name: this declares a
// function, as C++ reads it, and the call below reaches it.
int Value = 0;
Worker build(int n);

void LocalAliasHidesVariable() {
    using Value = int;
    Worker build(Value);
    build(1);
}

// A data member passed to a local's constructor is an argument, not a
// parameter type (`std::lock_guard<std::mutex> lock(mu_);`).
class Lock {
public:
    Lock(int &mutex) { (void)mutex; }
};

struct Counter {
    int mu_;
    void Tick() { Lock guard(mu_); }
};

// A pointer initialized from a local, spelled like a function declaration.
void OnTable() {}

void PointerFromLocal() {
    Callback table[1] = {OnTable};
    Callback *slot(table);
    (*slot)();
}

// Declarations that stay declarations: a parameter type, and no arguments.
void Declarations() {
    Worker make(Callback);
    Worker nothing();
}

// Doubled parentheses, the usual way to force an object, pass the function.
void OnParenthesized() {}

void Parenthesized() { Worker w((OnParenthesized)); }

// At file scope too: an object, and a pointer initialized from a function.
void OnGlobalObject() {}
void OnGlobalPointer() {}

Worker global_worker(OnGlobalObject);

static Callback static_callback = OnGlobalObject;
Worker static_worker(static_callback);

// A file `static` from an included header is a variable too.
void OnHeaderStatic() {}

#include "statics.h"

Worker header_worker(header_callback);

void HeaderLocal() { Worker w(header_callback); }
Callback global_callback(OnGlobalPointer);

void CallGlobalCallback() { global_callback(); }

// A file-scope declaration whose parameter is a type stays a declaration.
Worker make_global(Callback);
