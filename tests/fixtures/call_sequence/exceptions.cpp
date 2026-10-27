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
