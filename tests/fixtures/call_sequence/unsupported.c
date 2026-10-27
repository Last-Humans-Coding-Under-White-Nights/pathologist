typedef long jmp_buf[8];
int setjmp(jmp_buf env);
void longjmp(jmp_buf env, int value);
int a(void);
int b(void);
int c(void);
int g(void);
int k(void);
int f(int x, int y);

void statement_expression(void)
{
    int x = ({ int y = g(); y; });
    k();
}

void computed(void *target)
{
    a();
    goto *target;
    b();
}

void unresolved(void)
{
    a();
    goto nowhere;
    b();
}

void branchy(void)
{
    f(a() && b(), c());
}

void jumps(void)
{
    jmp_buf env;
    if (setjmp(env))
        a();
    longjmp(env, 1);
}
