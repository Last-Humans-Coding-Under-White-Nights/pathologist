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
