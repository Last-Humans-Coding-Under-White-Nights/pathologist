void a(void);
int d(void);
int e(void);
void c(void);

void broken(void)
{
    a();
    if (d() e()) {
    }
    c();
}
