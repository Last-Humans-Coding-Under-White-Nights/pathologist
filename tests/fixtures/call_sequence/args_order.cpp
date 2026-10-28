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

int sized();
int typed();
int checked();
int required();
int identified();
int bound();
void then();

void unevaluated_operands()
{
    sizeof(sized());
    decltype(typed()) v = 0;
    noexcept(checked());
    bool r = requires { required(); };
    typeid(identified());
    alignof(int[bound()]);
    then();
}
