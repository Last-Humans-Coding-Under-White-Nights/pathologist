int f();
int g();
int seed();
void body();

struct Base {
    Base(int v);
};

struct Derived : Base {
    Derived();
    int m;
};

Derived::Derived() : Base(f()), m(g())
{
    body();
}

struct Aggregate {
    int n;
    int v = seed();
};

struct Owner {
    Owner();
    Aggregate value;
};

Owner::Owner() : value{1}
{
    body();
}
