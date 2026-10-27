int check(void);
void fail(void);
void done(void);
void a(void);
void b(void);
void c(void);
void after(void);
void work(void);
void cleanup(void);

void early(void)
{
    if (check()) {
        fail();
        return;
    }
    done();
}

void cases(int x)
{
    switch (x) {
    case 1:
        a();
    case 2:
        b();
        break;
    case 3:
        c();
        break;
    }
    after();
}

void jump(void)
{
    if (check())
        goto out;
    work();
out:
    cleanup();
}
