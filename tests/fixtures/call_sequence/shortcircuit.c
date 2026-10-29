int a(void);
int b(void);
int g(void);
int h(void);
int k(void);
int p(void);
int q(void);
int f(int x, int y);
int f1(int x);

void both(void)
{
    if (a() && b())
        return;
}

void args(void)
{
    f(g(), h());
}

int sum(void)
{
    return f1(g()) + k();
}

void comma(void)
{
    (p(), q());
}
