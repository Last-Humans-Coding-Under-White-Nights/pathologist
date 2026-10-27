struct S {
    int m(int x);
};
S *make();
int arg();
int rhs();
int *lhs();

void receiver()
{
    make()->m(arg());
}

void assign()
{
    *lhs() = rhs();
}
