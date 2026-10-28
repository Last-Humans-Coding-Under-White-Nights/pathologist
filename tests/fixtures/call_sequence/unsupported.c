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

void computed_label(void *target)
{
    a();
    goto *target;
resume:
    b();
}

void computed_skip(void *target, int x)
{
    if (x)
        goto *target;
    a();
skip:
    b();
}

void long_jumped(void)
{
    jmp_buf env;
    if (setjmp(env))
        a();
    else
        b();
    longjmp(env, 1);
}

void resumed(void)
{
    jmp_buf env;
    if (setjmp(env))
        a();
    else
        b();
    c();
}

void hidden_goto(int x)
{
    if (x) {
        a();
        ({ goto target; 0; });
    } else {
    target:
        b();
    }
}
