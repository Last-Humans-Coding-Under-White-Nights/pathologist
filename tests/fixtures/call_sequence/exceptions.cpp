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
