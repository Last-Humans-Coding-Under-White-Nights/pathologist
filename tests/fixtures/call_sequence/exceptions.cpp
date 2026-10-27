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
