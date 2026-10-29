void a();
void b();
void h();
void c();
void before();
void dead();

void guarded()
{
    try {
        a();
        b();
    } catch (...) {
        h();
    }
    c();
}

void thrower()
{
    before();
    throw 1;
    dead();
}

struct Error {
};

void typed()
{
    try {
        a();
    } catch (Error &e) {
        h();
    }
}

Error work();

void rethrow()
{
    try {
        throw work();
    } catch (...) {
        h();
    }
}

bool k();
void d();
void e();

void split()
{
    try {
        a();
        if (k()) {
            b();
        }
        d();
    } catch (...) {
        h();
    }
    e();
}

void nested()
{
    try {
        try {
            a();
        } catch (...) {
        }
    } catch (...) {
        c();
    }
}

void nested_typed()
{
    try {
        try {
            a();
        } catch (Error &e) {
            h();
        }
    } catch (...) {
        c();
    }
}

struct Member {
    Member(int);
};
int make();
void run();

struct Guarded {
    Guarded();
    Member m_;
};

Guarded::Guarded()
try : m_(make()) {
    run();
} catch (...) {
    h();
}

typedef long jmp_buf[8];
int _setjmp(jmp_buf env);
void longjmp(jmp_buf env, int value);
namespace std {
int setjmp(jmp_buf env);
void longjmp(jmp_buf env, int value);
}
void initial();
void resumed();

void global_jump()
{
    jmp_buf env;
    if (::_setjmp(env)) {
        resumed();
    } else {
        initial();
        ::longjmp(env, 1);
    }
}

void std_jump()
{
    jmp_buf env;
    if (std::setjmp(env)) {
        resumed();
    } else {
        initial();
        std::longjmp(env, 1);
    }
}

template <typename... Ts> struct Fault {
};

template <typename... Ts> void variadic_catch()
{
    try {
        try {
            a();
        } catch (Fault<Ts...> &) {
        }
    } catch (...) {
        b();
    }
}

struct Sentry {
    Sentry();
    ~Sentry();
};
void may_throw();
void later();

void unwound()
{
    try {
        Sentry s;
        may_throw();
        later();
    } catch (...) {
        h();
    }
}

void unwound_nested()
{
    try {
        Sentry s;
        try {
            may_throw();
        } catch (Error &) {
        }
        later();
    } catch (...) {
        h();
    }
}

struct G {
    G();
    ~G();
    explicit operator bool() const;
};
struct E {
};
void w();
int n;

void dead_landing()
{
    G g;
    w();
    try {
        n = 1;
    } catch (E &) {
    }
    k();
}

void dead_after_return()
{
    w();
    try {
        return;
        later();
    } catch (E &) {
    }
}

void dead_after_break()
{
    G g;
    w();
    for (;;) {
        try {
            break;
            later();
        } catch (E &) {
        }
    }
    k();
}

void condition_if()
{
    try {
        if (G b = G())
            w();
    } catch (...) {
    }
}

void condition_while()
{
    try {
        while (G d = G())
            w();
    } catch (...) {
    }
}

void declared_object()
{
    try {
        G b = G();
        w();
    } catch (...) {
    }
}

void unevaluated_in_try()
{
    G g;
    w();
    try {
        n = sizeof(k());
    } catch (E &) {
    }
}

void thrown_pair()
{
    try {
        G a;
        G b;
        if (k())
            throw 1;
        w();
    } catch (...) {
        h();
    }
}
