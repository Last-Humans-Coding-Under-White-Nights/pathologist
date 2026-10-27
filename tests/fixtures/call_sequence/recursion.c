void leaf(void);

void walk(int n)
{
    if (n)
        walk(n - 1);
    leaf();
}
