struct ops {
    int (*m)(int);
};
struct ops *make(void);
int arg(void);
int rhs(void);
int *lhs(void);

void receiver(void)
{
    make()->m(arg());
}

void assign(void)
{
    *lhs() = rhs();
}

int cond(void);
int cond1(void);
int cond2(void);
int left(void);
int right(void);
int other(void);
int first(void);
int second(void);
int then1(void);
int else1(void);
int then2(void);
int else2(void);
int sink(int x, int y);

void ternary_first(void)
{
    sink(cond() ? left() : right(), other());
}

void ternary_last(void)
{
    sink(other(), cond() ? left() : right());
}

void two_ternaries(void)
{
    sink(cond1() ? then1() : else1(), first());
    sink(cond2() ? then2() : else2(), second());
}

int sized(void);
int counted(void);
void after_size(void);

void size_query(void)
{
    (void)sizeof sized();
    after_size();
}

void vla_size(void)
{
    (void)sizeof(int[counted()]);
    after_size();
}
